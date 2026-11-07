//! The few commands that run only with the user's yes: those whose harm
//! reaches past this computer, or past what a command's changes being
//! taken back can undo. Everything a command changes in the directories
//! the model may use can be undone, and Landlock keeps it from writing
//! elsewhere, so deleting or resetting files there is not on the list.

/// Why `line` needs the user's yes, if it does.
pub fn check(line: &str) -> Option<&'static str> {
    commands(line).iter().find_map(|c| why(c))
}

/// Each simple command of a line, as words, without the variable
/// settings and wrappers before the program.
fn commands(line: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for w in crate::shell::words(line) {
        if matches!(w.as_str(), "&&" | "||" | ";" | "\n" | "&" | "|" | "|&") {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(w);
        }
    }
    out.push(cur);
    let wrappers = [
        "sudo", "env", "command", "exec", "nohup", "time", "nice", "xargs",
    ];
    out.into_iter()
        .map(|c| {
            let skip = c
                .iter()
                .take_while(|w| {
                    (w.contains('=') && !w.starts_with('=') && !w.starts_with('-'))
                        || wrappers.contains(&w.as_str())
                })
                .count();
            c[skip..].to_vec()
        })
        .filter(|c| !c.is_empty())
        .collect()
}

const REMOTE: &str = "publishes, rewrites or deletes something outside this computer";
const INFRA: &str = "changes or destroys remote infrastructure";
const DATA: &str = "deletes data in a database";
const SYSTEM: &str = "stops processes or the system outside its own command";
const DISK: &str = "can wipe a disk";

fn why(c: &[String]) -> Option<&'static str> {
    let prog = c[0].rsplit('/').next().unwrap_or(&c[0]);
    let args: Vec<&str> = c[1..].iter().map(String::as_str).collect();
    let has = |w: &str| args.contains(&w);
    let any = |ws: &[&str]| args.iter().any(|a| ws.contains(a));
    let sql = || {
        let text = args.join(" ").to_ascii_uppercase();
        [
            "DROP ",
            "TRUNCATE ",
            "DELETE FROM",
            "FLUSHALL",
            "FLUSHDB",
            "DROPDATABASE",
        ]
        .iter()
        .any(|k| text.contains(k))
    };
    match prog {
        // A script given to a shell is checked as a line of its own.
        "sh" | "bash" | "zsh" | "dash" | "eval" => {
            let script = match prog {
                "eval" => Some(args.join(" ")),
                _ => args
                    .iter()
                    .position(|a| *a == "-c")
                    .and_then(|i| args.get(i + 1))
                    .map(|s| s.to_string()),
            };
            script.and_then(|s| check(&s))
        }
        "git" => {
            // Past git's own options, such as `-C dir`.
            let mut i = 0;
            while let Some(a) = args.get(i) {
                match *a {
                    "-C" | "-c" | "--git-dir" | "--work-tree" => i += 2,
                    a if a.starts_with('-') => i += 1,
                    _ => break,
                }
            }
            let rest = args.get(i + 1..).unwrap_or_default();
            match args.get(i) {
                Some(&"push") => rest
                    .iter()
                    .any(|a| {
                        matches!(
                            *a,
                            "-f" | "--force" | "--delete" | "-d" | "--mirror" | "--prune"
                        ) || a.starts_with("--force")
                            || (a.starts_with('+') && a.len() > 1)
                            || (a.starts_with(':') && a.len() > 1)
                    })
                    .then_some(REMOTE),
                _ => None,
            }
        }
        "cargo" | "npm" | "pnpm" | "yarn" | "poetry" | "flit" | "hatch" | "bun" => {
            any(&["publish", "unpublish", "yank", "deprecate"]).then_some(REMOTE)
        }
        "twine" => has("upload").then_some(REMOTE),
        "gem" => has("push").then_some(REMOTE),
        "docker" | "podman" => has("push").then_some(REMOTE),
        "gh" | "glab" => (any(&["delete", "merge", "close"]) || (has("release") && has("create")))
            .then_some(REMOTE),
        "rsync" => (has("--delete") && args.iter().any(|a| a.contains(':') && !a.starts_with('-')))
            .then_some(REMOTE),
        "ssh" => args
            .iter()
            .skip(1)
            .any(|a| {
                crate::shell::words(a)
                    .iter()
                    .any(|w| matches!(w.as_str(), "rm" | "rmdir" | "dd" | "mkfs"))
            })
            .then_some(REMOTE),
        "terraform" | "tofu" | "pulumi" | "cdk" => {
            any(&["destroy", "apply", "up", "deploy"]).then_some(INFRA)
        }
        "kubectl" | "oc" => {
            any(&["delete", "drain", "replace", "apply", "scale", "rollout"]).then_some(INFRA)
        }
        "helm" => any(&["uninstall", "delete", "upgrade", "install", "rollback"]).then_some(INFRA),
        "aws" | "gcloud" | "az" | "doctl" | "flyctl" | "fly" | "heroku" | "vercel" | "netlify"
        | "wrangler" => args
            .iter()
            .any(|a| {
                [
                    "delete",
                    "destroy",
                    "terminate",
                    "remove",
                    "rm",
                    "rb",
                    "deploy",
                    "purge",
                ]
                .iter()
                .any(|k| a.starts_with(k) || a.contains(&format!(":{k}")))
            })
            .then_some(INFRA),
        "psql" | "mysql" | "mariadb" | "mongo" | "mongosh" | "redis-cli" | "clickhouse-client" => {
            sql().then_some(DATA)
        }
        "dropdb" | "dropuser" => Some(DATA),
        "shutdown" | "reboot" | "poweroff" | "halt" | "pkill" | "killall" => Some(SYSTEM),
        // `-1` as the process: every process the user may signal.
        "kill" => (has("-1") && args.len() > 1).then_some(SYSTEM),
        "systemctl" => any(&[
            "poweroff",
            "reboot",
            "halt",
            "suspend",
            "hibernate",
            "stop",
            "kill",
            "disable",
            "mask",
            "isolate",
        ])
        .then_some(SYSTEM),
        "loginctl" => args
            .iter()
            .any(|a| {
                a.starts_with("terminate")
                    || a.starts_with("kill")
                    || matches!(*a, "poweroff" | "reboot")
            })
            .then_some(SYSTEM),
        "crontab" => has("-r").then_some(SYSTEM),
        "mkfs" | "wipefs" | "fdisk" | "sfdisk" | "parted" | "shred" => Some(DISK),
        p if p.starts_with("mkfs.") => Some(DISK),
        "dd" => args
            .iter()
            .any(|a| a.starts_with("of=/dev/"))
            .then_some(DISK),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_only_for_what_cannot_be_undone() {
        for line in [
            "git push --force origin main",
            "git -C repo push origin +main",
            "cd x && git push origin :old",
            "cargo publish",
            "FOO=1 npm publish --access public",
            "bash -c 'git push -f'",
            "terraform destroy -auto-approve",
            "kubectl delete pod x",
            "aws s3 rm s3://b/k",
            "psql -c 'DROP TABLE users'",
            "sudo reboot",
            "pkill -f server",
            "systemctl --user stop pipewire",
            "dd if=x of=/dev/sda",
            "gh repo delete me/x --yes",
            "kill -9 -1",
        ] {
            assert!(check(line).is_some(), "{line}");
        }
        for line in [
            "git push origin main",
            "git reset --hard HEAD~1",
            "git clean -fdx",
            "cargo build --release",
            "npm install",
            "echo 'git push --force' > notes.txt",
            "psql -c 'SELECT 1'",
            "kill 1234",
            "systemctl --user status x",
            "docker build .",
        ] {
            assert_eq!(check(line), None, "{line}");
        }
    }
}
