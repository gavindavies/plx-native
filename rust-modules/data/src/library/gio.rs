//! **Mounting an SMB share through gio (gvfs)**, and finding where its files are on disk.
//!
//! `gio mount smb://…` asks the gvfs daemon of the user's session to mount the share; gvfs-fuse
//! then shows it as an ordinary folder under `/run/user/<uid>/gvfs/`, which is what the library
//! lists and the player opens. `gio info` names that folder (`local path:`).
//!
//! The user and domain travel in the URI (`smb://DOMAIN;user@server/share`), so gio's only
//! question is the password. It asks on its stdout (`Password: `) and reads the answer from its
//! stdin, and so do its `User` and `Domain` questions when it asks them anyway: [`answer`] replies
//! to each by name. A share given no user and no password is mounted anonymously (`-a`). A second
//! password question means the first answer was refused; gio would ask forever, so it is stopped.
//!
//! Everything here blocks — it runs on the library's check worker, never on the frame — and every
//! gio is stopped after [`TIMEOUT`] (an unreachable server can take minutes to give up).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{Password, Share};

/// The program mounted with.
pub(super) const PROGRAM: &str = "gio";

/// How long one gio may take.
const TIMEOUT: Duration = Duration::from_secs(45);

/// **Mount `share`** (an already-mounted share counts) and return the local folder it shows as.
pub(super) fn mount(share: &Share, program: &str) -> Result<PathBuf, String> {
    let uri = credentialed_uri(share);
    let password = read_password(share)?;
    let anonymous = share.user.is_none() && password.is_none();
    let mut args = vec!["mount".to_owned()];
    if anonymous {
        args.push("-a".to_owned());
    }
    args.push(uri.clone());
    let answers = Answers { user: share.user.clone(), domain: share.domain.clone(), password };
    let run = run(program, &args, Some(&answers))?;
    if !run.ok && !run.stderr.contains("already mounted") {
        return Err(run.refused.unwrap_or_else(|| reason(&run.stderr, &uri)));
    }
    let info = run_quiet(program, &["info".to_owned(), uri.clone()])?;
    if !info.ok {
        return Err(reason(&info.stderr, &uri));
    }
    local_path(&info.stdout).ok_or_else(|| "mounted, but gvfs shows it at no local folder (is gvfs-fuse installed?)".to_owned())
}

/// The share's URI with its user and domain in it, each path segment escaped as a URI needs.
fn credentialed_uri(share: &Share) -> String {
    let rest = share.uri.strip_prefix("smb://").unwrap_or(&share.uri);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut uri = String::from("smb://");
    if !host.contains('@') {
        if let Some(user) = &share.user {
            if let Some(domain) = &share.domain {
                uri.push_str(&escape(domain));
                uri.push(';');
            }
            uri.push_str(&escape(user));
            uri.push('@');
        }
    }
    uri.push_str(host);
    for segment in path.split('/') {
        uri.push('/');
        uri.push_str(&escape(segment));
    }
    uri
}

/// Percent-escape everything a URI segment cannot hold as it is. A `%` is kept: the config may
/// already have escaped a name.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~%!$&'()*+,=".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn read_password(share: &Share) -> Result<Option<String>, String> {
    match &share.password {
        None => Ok(None),
        Some(Password::Inline(p)) => Ok(Some(p.clone())),
        Some(Password::File(path)) => std::fs::read_to_string(path)
            .map(|text| Some(text.lines().next().unwrap_or("").to_owned()))
            .map_err(|e| format!("cannot read the password file {}: {e}", path.display())),
    }
}

/// `gio info`'s `local path:` line.
fn local_path(info: &str) -> Option<PathBuf> {
    info.lines().find_map(|l| l.trim().strip_prefix("local path:")).map(|p| PathBuf::from(p.trim())).filter(|p| p.is_absolute())
}

/// gio's complaint, short enough for a row: the cause at the end of its last line
/// (`gio: smb://nas/x/: Failed to mount Windows share: Connection refused` → `Connection refused`).
/// The `uri` never reaches it, so neither can the user it carries.
fn reason(stderr: &str, uri: &str) -> String {
    let line = stderr.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("Gio failed without saying why");
    let line = line.replace(uri, "");
    let cause = line.rsplit(": ").map(str::trim).find(|part| !part.is_empty() && *part != "/").unwrap_or("Gio failed without saying why");
    let mut chars = cause.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// What gio is told when it asks.
struct Answers {
    user: Option<String>,
    domain: Option<String>,
    password: Option<String>,
}

/// The reply to the question gio has printed so far, `None` while it is not a question yet. An
/// empty answer takes gio's default (the `[…]` it offers).
fn answer<'a>(asked: &str, answers: &'a Answers, passwords_asked: &mut u32) -> Option<Result<&'a str, String>> {
    let asked = asked.trim_start();
    if !asked.ends_with(": ") {
        return None;
    }
    if asked.starts_with("Password") {
        *passwords_asked += 1;
        return Some(match (&answers.password, *passwords_asked) {
            (_, 2..) => Err("the share refused the user name or password".to_owned()),
            (Some(p), _) => Ok(p.as_str()),
            (None, _) => Ok(""),
        });
    }
    if asked.starts_with("User") {
        return Some(Ok(answers.user.as_deref().unwrap_or("")));
    }
    if asked.starts_with("Domain") {
        return Some(Ok(answers.domain.as_deref().unwrap_or("")));
    }
    None
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
    /// Set when the run was stopped because a password was refused.
    refused: Option<String>,
}

fn run_quiet(program: &str, args: &[String]) -> Result<Run, String> {
    run(program, args, None)
}

/// Run gio, answering its questions from `answers` (none: its stdin is closed), and stop it at
/// [`TIMEOUT`].
fn run(program: &str, args: &[String], answers: Option<&Answers>) -> Result<Run, String> {
    let child = Command::new(program)
        .args(args)
        .env("LC_ALL", "C") // its questions and complaints in the words `answer` reads
        .stdin(if answers.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    let child = Arc::new(Mutex::new(child));
    let (mut stdin, mut stdout, mut stderr) = {
        let mut c = child.lock().unwrap_or_else(|e| e.into_inner());
        (c.stdin.take(), c.stdout.take(), c.stderr.take())
    };

    // the watchdog: a blocked read below ends when the child is killed
    let done = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));
    {
        let (child, done, timed_out) = (child.clone(), done.clone(), timed_out.clone());
        plx_base::task::spawn_small("gio watchdog", move || {
            let deadline = Instant::now() + TIMEOUT;
            while !done.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    timed_out.store(true, Ordering::Release);
                    let _ = child.lock().unwrap_or_else(|e| e.into_inner()).kill();
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
    }

    let mut out = Vec::new();
    let mut refused = None;
    if let Some(pipe) = stdout.as_mut() {
        let mut asked = String::new();
        let mut passwords_asked = 0;
        let mut buf = [0u8; 256];
        loop {
            let n = match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            out.extend_from_slice(&buf[..n]);
            for &b in &buf[..n] {
                if b == b'\n' {
                    asked.clear();
                } else {
                    asked.push(b as char);
                }
            }
            let Some(answers) = answers else { continue };
            match answer(&asked, answers, &mut passwords_asked) {
                None => {}
                Some(Ok(reply)) => {
                    asked.clear();
                    if let Some(input) = stdin.as_mut() {
                        let _ = input.write_all(format!("{reply}\n").as_bytes());
                        let _ = input.flush();
                    }
                }
                Some(Err(why)) => {
                    refused = Some(why);
                    let _ = child.lock().unwrap_or_else(|e| e.into_inner()).kill();
                    break;
                }
            }
        }
    }
    drop(stdin.take());
    let mut err = String::new();
    if let Some(pipe) = stderr.as_mut() {
        let _ = pipe.read_to_string(&mut err);
    }
    let status = loop {
        if let Some(status) = child.lock().unwrap_or_else(|e| e.into_inner()).try_wait().ok().flatten() {
            break Some(status);
        }
        if timed_out.load(Ordering::Acquire) {
            let _ = child.lock().unwrap_or_else(|e| e.into_inner()).wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    done.store(true, Ordering::Release);
    if timed_out.load(Ordering::Acquire) {
        return Err(format!("no answer from the server in {} s", TIMEOUT.as_secs()));
    }
    Ok(Run {
        ok: status.is_some_and(|s| s.success()) && refused.is_none(),
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: err,
        refused,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(uri: &str, user: Option<&str>, domain: Option<&str>) -> Share {
        Share { uri: uri.into(), user: user.map(Into::into), domain: domain.map(Into::into), password: None }
    }

    #[test]
    fn the_user_and_domain_ride_in_the_uri_and_names_are_escaped() {
        assert_eq!(credentialed_uri(&share("smb://nas/media/TV Shows", None, None)), "smb://nas/media/TV%20Shows");
        assert_eq!(credentialed_uri(&share("smb://nas/media", Some("gavin"), None)), "smb://gavin@nas/media");
        assert_eq!(credentialed_uri(&share("smb://nas/media/", Some("a b@c"), Some("WORK"))), "smb://WORK;a%20b%40c@nas/media/");
        assert_eq!(credentialed_uri(&share("smb://me@nas/media", Some("other"), None)), "smb://me@nas/media", "a user already in the URI stays");
        assert_eq!(credentialed_uri(&share("smb://nas/Films%20HD", None, None)), "smb://nas/Films%20HD");
    }

    #[test]
    fn each_question_gets_its_answer_and_a_second_password_question_stops() {
        let answers = Answers { user: Some("gavin".into()), domain: None, password: Some("pw".into()) };
        let mut n = 0;
        assert_eq!(answer("Password", &answers, &mut n), None, "not a question until it ends `: `");
        assert_eq!(answer("User [root]: ", &answers, &mut n), Some(Ok("gavin")));
        assert_eq!(answer("Domain [WORKGROUP]: ", &answers, &mut n), Some(Ok("")), "empty takes the default");
        assert_eq!(answer("Password: ", &answers, &mut n), Some(Ok("pw")));
        assert!(matches!(answer("Password: ", &answers, &mut n), Some(Err(_))));
        assert_eq!(answer("Something else: ", &answers, &mut 0), None);
    }

    #[test]
    fn gio_info_names_the_local_folder_and_a_complaint_loses_its_prefix() {
        let info = "display name: media on nas\nname: media\ntype: directory\nlocal path: /run/user/1000/gvfs/smb-share:server=nas,share=media\nuri: smb://nas/media/\n";
        assert_eq!(local_path(info), Some(PathBuf::from("/run/user/1000/gvfs/smb-share:server=nas,share=media")));
        assert_eq!(local_path("name: x\n"), None);
        assert_eq!(
            reason("gio: smb://nas/media/: Failed to mount Windows share: Connection refused\n", "smb://nas/media"),
            "Connection refused",
            "gio echoes the URI with a trailing slash"
        );
        assert_eq!(reason("gio: smb://gavin@nas/x: permission denied\n", "smb://gavin@nas/x"), "Permission denied");
        assert_eq!(reason("", "smb://x/y"), "Gio failed without saying why");
    }

    /// A stand-in gio: asks for a password, accepts `secret`, and answers `info` with a local path.
    fn fake_gio(dir: &std::path::Path) -> String {
        let script = dir.join("gio");
        std::fs::write(
            &script,
            "#!/bin/sh\ncase \"$1\" in\n mount)\n  for a in \"$@\"; do last=\"$a\"; done\n  printf 'Password: '\n  read pw || exit 2\n  [ \"$pw\" = secret ] && exit 0\n  printf 'Password: '\n  read pw\n  exit 2;;\n info) echo \"local path: /run/gvfs/$2\"; echo \"uri: $2\";;\nesac\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script.display().to_string()
    }

    #[test]
    fn a_share_mounts_through_gio_with_its_password_and_a_wrong_one_is_refused() {
        let dir = plx_base::testscratch::process_dir("gio-fake");
        let gio = fake_gio(&dir);
        std::fs::write(dir.join("pass"), "secret\n").unwrap();
        let mut s = share("smb://nas/media", Some("gavin"), None);
        s.password = Some(Password::File(dir.join("pass")));
        assert_eq!(mount(&s, &gio), Ok(PathBuf::from("/run/gvfs/smb://gavin@nas/media")));

        s.password = Some(Password::Inline("wrong".into()));
        assert_eq!(mount(&s, &gio), Err("the share refused the user name or password".to_owned()));

        s.password = Some(Password::File(dir.join("missing")));
        assert!(mount(&s, &gio).unwrap_err().starts_with("cannot read the password file"));

        s.password = Some(Password::Inline("secret".into()));
        assert!(mount(&s, "/nonexistent/gio").unwrap_err().starts_with("cannot run"));
    }
}
