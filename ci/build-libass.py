#!/usr/bin/env python3
"""Build the private ASS facade with a pinned, statically contained font stack.

Only plx_ass_* leaves the resulting shared library. No system FreeType, FriBidi,
HarfBuzz, libass, or platform font provider can affect its ABI or rendering.
Built for this machine (Linux) and bundled beside the binary as pkg/libass-plx-host.so.0.
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import posixpath
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PINS = ROOT / 'ci/libass-dependencies.json'


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(args, *, cwd=None, env=None):
    subprocess.run([str(x) for x in args], cwd=cwd, env=env, check=True)


# Per-source bound on the curl step: a dead host must hand over to the next source quickly.
CURL_BOUNDS = ['--connect-timeout', '15', '--retry', '2', '--retry-delay', '2', '--max-time', '300']


def sources_of(dep):
    """The canonical upstream URL first, then transport-only fallbacks. Every one must yield
    the same pinned bytes; a mirror is never a different release."""
    mirrors = dep.get('mirrors', [])
    if not isinstance(mirrors, list) or not all(isinstance(url, str) and url for url in mirrors):
        raise ValueError('mirrors of ' + dep['id'] + ' must be a list of URLs')
    return [dep['url'], *mirrors]


def archive(dep, folder):
    target = folder / dep['archive']
    if target.is_file() and digest(target) == dep['sha256']:
        return target
    # Never publish a partial pathname.
    fd, candidate = tempfile.mkstemp(prefix=target.name + '.', dir=folder)
    os.close(fd)
    failures = []
    try:
        for url in sources_of(dep):
            result = subprocess.run(['curl', '-fsSL', *CURL_BOUNDS, url, '-o', candidate])
            if result.returncode != 0:
                failures.append(url + ' (curl exit ' + str(result.returncode) + ')')
                print('libass: ' + dep['archive'] + ' not available from ' + url +
                      ' (curl exit ' + str(result.returncode) + '); trying the next source', flush=True)
                continue
            # A source that answers with the wrong bytes is never skipped past.
            if digest(Path(candidate)) != dep['sha256']:
                raise ValueError('source checksum mismatch: ' + dep['archive'] + ' from ' + url)
            os.replace(candidate, target)
            print('libass: ' + dep['archive'] + ' fetched from ' + url, flush=True)
            return target
        raise ValueError('could not download ' + dep['archive'] + ' from any source: ' + '; '.join(failures))
    finally:
        Path(candidate).unlink(missing_ok=True)


def extract(path, folder):
    # Reject escaping names/links before extracting the checksum-pinned upstream release.
    with tarfile.open(path) as contents:
        for entry in contents:
            name = posixpath.normpath(entry.name)
            if name.startswith('/') or name == '..' or name.startswith('../'):
                raise ValueError('unsafe source archive path')
            if entry.issym() or entry.islnk():
                name = posixpath.normpath(posixpath.join(posixpath.dirname(name), entry.linkname)
                                         if entry.issym() else entry.linkname)
                if name.startswith('/') or name == '..' or name.startswith('../'):
                    raise ValueError('unsafe source archive link')
            elif not entry.isfile() and not entry.isdir():
                raise ValueError('special source archive member')
        contents.extractall(folder)


def verify_library(path):
    """Prove the bundled font stack stayed private and needs no C++ runtime."""
    readelf = shutil.which('readelf')
    dynamic = subprocess.check_output([readelf, '-d', str(path)], text=True)
    needed = re.findall(r'\(NEEDED\).*\[([^\]]+)\]', dynamic)
    allowed = {'libc.so.6', 'libm.so.6', 'libgcc_s.so.1', 'libpthread.so.0'}
    symbols = subprocess.check_output([readelf, '--dyn-syms', '--wide', str(path)], text=True)
    names = []
    for line in symbols.splitlines():
        fields = line.split()
        if len(fields) >= 8 and fields[4] in {'GLOBAL', 'WEAK'} and fields[6] != 'UND':
            names.append(fields[7])
    unexpected = set(needed) - allowed
    if unexpected:
        raise ValueError('unexpected runtime dependencies: ' + ', '.join(sorted(unexpected)))
    if not names or any(not name.startswith('plx_ass_') for name in names):
        raise ValueError('font stack leaked a symbol outside the private ASS facade')
    print('libass: dependency/export isolation PASS', flush=True)


def build():
    prefix = ROOT / 'vendor/libass-prefix-host'
    work = ROOT / 'vendor/libass-build-host'
    sources = ROOT / 'vendor/libass-sources'
    for directory in [work, sources, prefix, ROOT / 'pkg']:
        directory.mkdir(parents=True, exist_ok=True)
    # A kernel-held lock disappears with its owner; no stale pid-file recovery.
    with (work / '.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        build_locked(prefix, work, sources)


def build_locked(prefix, work, sources):
    pins = json.loads(PINS.read_text())
    archives = {d['id']: archive(d, sources) for d in pins}
    env = os.environ.copy()
    # Avoid package auto-detection and caller-supplied flags changing the stack.
    # The key records the compiler; all build flags are owned here.
    for key in ['CFLAGS', 'CXXFLAGS', 'CPPFLAGS', 'LDFLAGS', 'LIBS',
                'PKG_CONFIG_PATH', 'PKG_CONFIG_SYSROOT_DIR']:
        env.pop(key, None)
    env['PKG_CONFIG_LIBDIR'] = str(prefix / 'lib/pkgconfig')
    cmake = shutil.which('cmake')
    pkgconfig = shutil.which('pkg-config')
    if not cmake or not pkgconfig:
        raise ValueError('cmake and pkg-config are required')
    flags = ['-O2', '-fPIC', '-fvisibility=hidden', '-funwind-tables', '-fno-omit-frame-pointer',
             '-ffile-prefix-map=' + str(ROOT) + '=/pitv']
    cmake_flags = ['-DCMAKE_BUILD_TYPE=Release', '-DBUILD_SHARED_LIBS=OFF',
                   '-DCMAKE_POSITION_INDEPENDENT_CODE=ON', '-DCMAKE_INSTALL_LIBDIR=lib',
                   '-DCMAKE_PREFIX_PATH=' + str(prefix),
                   '-DCMAKE_INSTALL_PREFIX=' + str(prefix)]
    configure_flags = ['--prefix=' + str(prefix), '--libdir=' + str(prefix / 'lib'),
                       '--disable-shared', '--enable-static', '--with-pic']
    cc, cxx = shutil.which('cc'), shutil.which('c++')
    ar, ranlib = shutil.which('ar'), shutil.which('ranlib')
    if not all([cc, cxx, ar, ranlib]):
        raise ValueError('a C/C++ compiler, ar and ranlib are required')
    identity = subprocess.check_output([cc, '--version'])
    env.update(CC=cc, CXX=cxx, AR=ar, RANLIB=ranlib,
               CFLAGS=shlex.join(flags), CXXFLAGS=shlex.join(flags))
    cmake_flags += ['-DCMAKE_C_COMPILER=' + cc, '-DCMAKE_CXX_COMPILER=' + cxx,
                    '-DCMAKE_AR=' + ar, '-DCMAKE_RANLIB=' + ranlib,
                    '-DCMAKE_C_FLAGS=' + shlex.join(flags),
                    '-DCMAKE_CXX_FLAGS=' + shlex.join(flags)]
    key = hashlib.sha256(PINS.read_bytes() + Path(__file__).read_bytes() + identity +
                         json.dumps([platform.machine(), cmake_flags, configure_flags]).encode()).hexdigest()
    stamp = prefix / '.dependencies-key'
    static_names = ['libass.a', 'libharfbuzz.a', 'libfribidi.a', 'libfreetype.a']
    if not stamp.is_file() or stamp.read_text() != key or not all((prefix / 'lib' / n).is_file() for n in static_names):
        for name in ['source', 'objects']:
            shutil.rmtree(work / name, ignore_errors=True)
            (work / name).mkdir()
        # This directory contains only this recipe's derived output.
        shutil.rmtree(prefix)
        prefix.mkdir()
        for dep in pins:
            extract(archives[dep['id']], work / 'source')
        by_id = {d['id']: work / 'source' / (d['id'] + '-' + d['version']) for d in pins}
        jobs = os.environ.get('JOBS', str(min(os.cpu_count() or 4, 8)))

        def cmake_build(name, options):
            directory = work / 'objects' / name
            run([cmake, '-S', by_id[name], '-B', directory, *cmake_flags, *options], env=env)
            run([cmake, '--build', directory, '--parallel', jobs], env=env)
            run([cmake, '--install', directory], env=env)

        def autotools_build(name, options):
            directory = work / 'objects' / name
            directory.mkdir()
            run([by_id[name] / 'configure', *configure_flags, *options], cwd=directory, env=env)
            if name == 'fribidi':
                # Its release contains generated Unicode tables. Build the library alone;
                # the CLI/manpage targets need help2man.
                run(['make', '-C', 'lib', '-j' + jobs], cwd=directory, env=env)
                run(['make', '-C', 'lib', 'install'], cwd=directory, env=env)
                run(['make', 'install-pkgconfigDATA'], cwd=directory, env=env)
            else:
                run(['make', '-j' + jobs], cwd=directory, env=env)
                run(['make', 'install'], cwd=directory, env=env)

        cmake_build('freetype', ['-DFT_DISABLE_' + dep + '=ON'
                               for dep in ['ZLIB', 'BZIP2', 'PNG', 'HARFBUZZ', 'BROTLI']])
        autotools_build('fribidi', [])
        cmake_build('harfbuzz', ['-DHB_HAVE_FREETYPE=ON', '-DFREETYPE_LIBRARY=' + str(prefix / 'lib/libfreetype.a'),
                                '-DFREETYPE_INCLUDE_DIRS=' + str(prefix / 'include/freetype2'),
                                '-DHB_HAVE_CORETEXT=OFF', '-DHB_BUILD_UTILS=OFF',
                                '-DHB_BUILD_SUBSET=OFF', '-DHB_BUILD_RASTER=OFF',
                                '-DHB_BUILD_VECTOR=OFF', '-DHB_BUILD_GPU=OFF'])
        autotools_build('libass', ['--disable-fontconfig', '--disable-coretext', '--disable-directwrite',
                                  '--disable-require-system-font-provider', '--disable-libunibreak',
                                  '--disable-test', '--disable-compare', '--disable-profile'])
        stamp.write_text(key)
    # The facade changes more often than the upstream stack; relinking it does not
    # require a clean FreeType/HarfBuzz rebuild.
    name = 'libass-plx-host.so.0'
    output = prefix / 'lib' / name
    objects = [prefix / 'lib' / n for n in static_names]
    wrapper_flags = [f for f in flags if f != '-fvisibility=hidden']
    exports = work / 'exports.map'
    exports.write_text('{ global: plx_ass_*; local: *; };\n')
    command = [cc, *wrapper_flags, '-I' + str(ROOT / 'include'), '-I' + str(prefix / 'include'),
               str(ROOT / 'src/ass.c'), *map(str, objects), '-lm',
               '-shared', '-Wl,-soname,' + name, '-Wl,--no-undefined',
               '-Wl,--exclude-libs,ALL', '-Wl,--version-script=' + str(exports)]
    run([*command, '-o', output], env=env)
    verify_library(output)
    staged = ROOT / 'pkg' / name
    candidate = staged.with_name(staged.name + '.' + str(os.getpid()) + '.new')
    try:
        shutil.copy2(output, candidate)
        candidate.chmod(0o755)
        # Publish the target last, so a failed step never leaves a new-enough Make target.
        os.replace(candidate, staged)
    finally:
        candidate.unlink(missing_ok=True)
    print('libass: ' + str(staged.relative_to(ROOT)), flush=True)


if __name__ == '__main__':
    try:
        build()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        sys.exit('build-libass: ' + str(error))
