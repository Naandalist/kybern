//! The text of each platform's login item. Pure functions, so every one is tested on every
//! platform; each is compiled only where it is used, and under test.

#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
use super::ServiceSpec;

/// What the login item's description says, in Task Scheduler, launchd and systemd.
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
pub const DESCRIPTION: &str = "Keeps Kybern reachable from your other machines while the app is closed.";

/// `&`, `<`, `>`, `"` and `'` for an XML text node or attribute (Task Scheduler, plists).
#[cfg(any(windows, target_os = "macos", test))]
pub fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// One argument as `CommandLineToArgvW` reads it back: quoted when it has spaces, tabs or quotes,
/// with backslashes doubled only before a quote.
#[cfg(any(windows, test))]
pub fn windows_quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|ch| matches!(ch, ' ' | '\t' | '"')) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(ch);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// The task action's arguments: `conhost.exe --headless <kybernd> <args>`.
#[cfg(any(windows, test))]
pub fn task_arguments(spec: &ServiceSpec) -> String {
    let mut arguments = vec!["--headless".to_string(), windows_quote(&spec.exe.to_string_lossy())];
    arguments.extend(spec.args().iter().map(|arg| windows_quote(arg)));
    arguments.join(" ")
}

/// The Task Scheduler definition (Windows). A logon trigger for this user only, which a standard
/// user may register without elevation; an interactive token, so the daemon runs in the user's
/// session (the clipboard and computer use need it); no time limit (the default ends a task after
/// three days); normal priority (the default 7 also lowers I/O and memory priority); runs on battery.
///
/// The action is `conhost.exe --headless`, which hosts the console daemon without a window: the
/// task starting `kybernd.exe` itself would open a console window at every login.
#[cfg(any(windows, test))]
pub fn task_xml(spec: &ServiceSpec, user: &str, conhost: &str) -> String {
    let user = xml_escape(user);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>Kybern</Author>
    <Description>{description}</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        description = xml_escape(DESCRIPTION),
        command = xml_escape(conhost),
        arguments = xml_escape(&task_arguments(spec)),
    )
}

/// The LaunchAgent (macOS). `RunAtLoad` starts it at login. `KeepAlive` restarts it only after a
/// crash: a daemon that exits cleanly (the app replacing it with a newer binary, a superseded or
/// idle exit) stays stopped, so launchd never races the app for the data folder.
#[cfg(any(target_os = "macos", test))]
pub fn launch_agent_plist(spec: &ServiceSpec) -> String {
    let mut program = vec![spec.exe.to_string_lossy().into_owned()];
    program.extend(spec.args());
    let arguments: String = program.iter().map(|arg| format!("    <string>{}</string>\n", xml_escape(arg))).collect();
    let log = xml_escape(&spec.data_dir.join("daemon.log").to_string_lossy());
    let environment = spec
        .path_env
        .as_deref()
        .map(|path| {
            format!(
                "  <key>EnvironmentVariables</key>\n  <dict>\n    <key>PATH</key>\n    <string>{}</string>\n  </dict>\n",
                xml_escape(path)
            )
        })
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>Comment</key>
  <string>{description}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}  </array>
{environment}  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        label = xml_escape(&spec.name),
        description = xml_escape(DESCRIPTION),
    )
}

/// A systemd `ExecStart=` word: quoted, with `\`, `"`, the specifier `%` and the variable `$` escaped.
#[cfg(any(target_os = "linux", test))]
fn systemd_quote(arg: &str) -> String {
    // `$` starts a variable even inside quotes; `$$` is a literal one.
    let escaped = arg.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$");
    format!("\"{escaped}\"")
}

/// What every unit Kybern writes says about the out-of-memory killer. systemd's default
/// `OOMPolicy=stop` stops the whole service when the kernel kills any process in its cgroup, so one
/// agent's build that runs out of memory took the daemon and every other agent with it (instabox,
/// 2026-11-02). `continue` leaves the rest running.
///
/// `OOMScoreAdjust` is not set: every process in the unit inherits it, so it cannot make children die
/// before the daemon, and an unprivileged user may only raise it. The kernel already picks the
/// largest process, which is an agent's build, not the daemon.
#[cfg(any(target_os = "linux", test))]
pub const OOM_POLICY: &str = "OOMPolicy=continue";

/// The `systemd --user` unit (Linux). Started at login by `default.target`, restarted only after a
/// failure, so a clean exit (the app replacing it, a superseded exit) stays stopped. `KillSignal=SIGINT`
/// is the daemon's graceful shutdown.
#[cfg(any(target_os = "linux", test))]
pub fn systemd_unit(spec: &ServiceSpec) -> String {
    let mut exec = vec![systemd_quote(&spec.exe.to_string_lossy())];
    exec.extend(spec.args().iter().map(|arg| systemd_quote(arg)));
    let environment =
        spec.path_env.as_deref().map(|path| format!("Environment={}\n", systemd_quote(&format!("PATH={path}")))).unwrap_or_default();
    format!(
        "[Unit]\nDescription={DESCRIPTION}\n\n[Service]\nType=simple\nExecStart={exec}\nWorkingDirectory=%h\n{environment}Restart=on-failure\nRestartSec=5\nKillSignal=SIGINT\nTimeoutStopSec=30\n{OOM_POLICY}\n\n[Install]\nWantedBy=default.target\n",
        exec = exec.join(" "),
    )
}

/// The XDG autostart entry, where there is no user systemd (Linux). The desktop session starts it
/// at login; nothing restarts it.
#[cfg(any(target_os = "linux", test))]
pub fn autostart_desktop(spec: &ServiceSpec) -> String {
    // Desktop Entry `Exec`: double-quote each argument, escape `"`, `` ` ``, `$` and `\` inside, and
    // double `%`, which starts a field code.
    let quote = |arg: &str| {
        let mut out = String::from("\"");
        for ch in arg.chars() {
            match ch {
                // The value is unescaped once as a string (`\\` is `\`) before the quoting rules
                // apply, so a literal backslash takes four.
                '\\' => out.push_str("\\\\\\\\"),
                '"' | '`' | '$' => {
                    out.push('\\');
                    out.push(ch);
                }
                '%' => out.push_str("%%"),
                _ => out.push(ch),
            }
        }
        out.push('"');
        out
    };
    let mut exec = vec![quote(&spec.exe.to_string_lossy())];
    exec.extend(spec.args().iter().map(|arg| quote(arg)));
    format!(
        "[Desktop Entry]\nType=Application\nName=Kybern daemon\nComment={DESCRIPTION}\nExec={exec}\nTerminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
        exec = exec.join(" "),
    )
}

/// Whether a unit's `ExecStart` runs a daemon for `data_dir` (quoted or not).
#[cfg(any(target_os = "linux", test))]
pub fn starts_data_dir(unit: &str, data_dir: &str) -> bool {
    unit.lines().filter(|line| line.trim_start().starts_with("ExecStart=")).any(|line| {
        // The remote installers write plain words (their paths have no spaces); ours quotes every word.
        let words: Vec<&str> = line.split_whitespace().map(|word| word.trim_matches('"')).collect();
        words.windows(2).any(|pair| pair[0] == "--data-dir" && pair[1] == data_dir)
            || line.contains(&format!("\"--data-dir\" \"{data_dir}\""))
    })
}

/// The drop-in that gives a unit written before [`OOM_POLICY`] the policy without rewriting it.
#[cfg(any(target_os = "linux", test))]
pub const OOM_DROP_IN: &str = "[Service]\nOOMPolicy=continue\n";

/// Whether a Kybern-written unit needs the [`OOM_DROP_IN`]: it is ours (the remote installers'
/// description), it sets no `OOMPolicy` of its own, and no drop-in sets one either.
#[cfg(any(target_os = "linux", test))]
pub fn needs_oom_drop_in(unit: &str, drop_ins: &[String]) -> bool {
    let sets_policy = |text: &str| text.lines().any(|line| line.trim_start().starts_with("OOMPolicy="));
    let ours = unit.lines().any(|line| {
        let line = line.trim();
        line == "Description=Kybern agent daemon" || line == format!("Description={DESCRIPTION}")
    });
    ours && !sets_policy(unit) && !drop_ins.iter().any(|text| sets_policy(text))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn spec(exe: &str, data: &str) -> ServiceSpec {
        ServiceSpec { name: "dev.kybern.kybernd.login".into(), exe: PathBuf::from(exe), data_dir: PathBuf::from(data), path_env: None }
    }

    #[test]
    fn windows_arguments_survive_spaces_quotes_and_trailing_backslashes() {
        assert_eq!(windows_quote("plain"), "plain");
        assert_eq!(windows_quote(r"C:\Program Files\Kybern\kybernd.exe"), r#""C:\Program Files\Kybern\kybernd.exe""#);
        assert_eq!(windows_quote(r"C:\data dir\"), r#""C:\data dir\\""#);
        assert_eq!(windows_quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(windows_quote(""), r#""""#);
    }

    #[test]
    fn the_task_runs_the_daemon_headless_at_this_users_logon_with_no_time_limit() {
        let xml = task_xml(
            &spec(r"C:\Users\Bagus\AppData\Local\Programs\Kybern\kybernd.exe", r"C:\Users\Bagus\.kybern"),
            r"SATYAMAS\bagus",
            r"C:\Windows\System32\conhost.exe",
        );
        assert!(xml.contains(r"<UserId>SATYAMAS\bagus</UserId>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"), "never elevated");
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains(r"<Command>C:\Windows\System32\conhost.exe</Command>"));
        assert!(xml.contains(
            r"<Arguments>--headless C:\Users\Bagus\AppData\Local\Programs\Kybern\kybernd.exe --data-dir C:\Users\Bagus\.kybern --port 0 --login-service</Arguments>"
        ));
    }

    #[test]
    fn the_task_escapes_xml_in_paths_and_user_names() {
        let xml = task_xml(&spec(r"C:\R&D <x>\kybernd.exe", r"C:\d"), r"DOM\o'neil", r"C:\Windows\System32\conhost.exe");
        assert!(xml.contains(r#"<Arguments>--headless &quot;C:\R&amp;D &lt;x&gt;\kybernd.exe&quot; --data-dir"#), "{xml}");
        assert!(xml.contains(r"<UserId>DOM\o&apos;neil</UserId>"));
    }

    #[test]
    fn the_launch_agent_starts_at_login_and_restarts_only_after_a_crash() {
        let mut agent = spec("/Applications/Kybern.app/Contents/MacOS/kybernd", "/Users/me/.kybern");
        agent.path_env = Some("/opt/homebrew/bin:/usr/bin".into());
        let plist = launch_agent_plist(&agent);
        assert!(plist.contains("<key>Label</key>\n  <string>dev.kybern.kybernd.login</string>"));
        assert!(plist.contains(
            "    <string>/Applications/Kybern.app/Contents/MacOS/kybernd</string>\n    <string>--data-dir</string>\n    <string>/Users/me/.kybern</string>\n    <string>--port</string>\n    <string>0</string>\n    <string>--login-service</string>\n"
        ));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(plist.contains("<key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>"));
        assert!(plist.contains("<key>PATH</key>\n    <string>/opt/homebrew/bin:/usr/bin</string>"));
        let log = agent.data_dir.join("daemon.log");
        assert!(plist.contains(&format!("<string>{}</string>", log.display())));
        let bare = launch_agent_plist(&spec("/a & b/kybernd", "/d"));
        assert!(bare.contains("<string>/a &amp; b/kybernd</string>") && !bare.contains("EnvironmentVariables"));
    }

    #[test]
    fn the_systemd_unit_quotes_its_command_and_survives_an_agent_running_out_of_memory() {
        let mut unit = spec("/home/me/.local/share/kybern gpuix/kybernd", "/home/me/.kybern");
        unit.path_env = Some("/home/me/.cargo/bin:/usr/bin".into());
        let text = systemd_unit(&unit);
        assert!(text.contains(
            "ExecStart=\"/home/me/.local/share/kybern gpuix/kybernd\" \"--data-dir\" \"/home/me/.kybern\" \"--port\" \"0\" \"--login-service\"\n"
        ));
        assert!(text.contains("Environment=\"PATH=/home/me/.cargo/bin:/usr/bin\"\n"));
        assert!(text.contains("\nOOMPolicy=continue\n"), "{text}");
        assert!(text.contains("\nRestart=on-failure\n") && text.contains("\nWantedBy=default.target\n"));
        assert!(!text.contains("OOMScoreAdjust"));
        assert!(systemd_unit(&spec("/x/100%/kybernd", "/d")).contains("\"/x/100%%/kybernd\""));
        assert!(systemd_unit(&spec("/x/$HOME/kybernd", "/d")).contains("\"/x/$$HOME/kybernd\""), "no variable expansion");
        let unit = systemd_unit(&spec("/x/kybernd", "/home/me/.kybern"));
        assert!(starts_data_dir(&unit, "/home/me/.kybern") && !starts_data_dir(&unit, "/home/me/.kyb"));
        assert!(starts_data_dir("ExecStart=/b/kybernd --bind 127.0.0.1 --port 4173 --data-dir /srv/k\n", "/srv/k"));
        assert!(!starts_data_dir("ExecStart=/b/kybernd --data-dir /srv/kk\n", "/srv/k"), "not a prefix");
        assert!(starts_data_dir(&systemd_unit(&spec("/x/kybernd", "/home/me/my data")), "/home/me/my data"));
    }

    #[test]
    fn the_autostart_entry_escapes_its_command_line() {
        assert!(autostart_desktop(&spec("/opt/a\\b/kybernd", "/d")).contains("Exec=\"/opt/a\\\\\\\\b/kybernd\""));
        let text = autostart_desktop(&spec("/opt/my $app/kybernd", "/home/me/50%"));
        assert!(
            text.contains("Exec=\"/opt/my \\$app/kybernd\" \"--data-dir\" \"/home/me/50%%\" \"--port\" \"0\" \"--login-service\"\n"),
            "{text}"
        );
        assert!(text.contains("\nType=Application\n") && text.contains("\nNoDisplay=true\n"));
    }

    #[test]
    fn only_a_kybern_unit_without_a_policy_gets_the_oom_drop_in() {
        let installer = "[Unit]\nDescription=Kybern agent daemon\n[Service]\nExecStart=/x/kybernd\nRestart=on-failure\n";
        assert!(needs_oom_drop_in(installer, &[]));
        assert!(!needs_oom_drop_in(installer, &[OOM_DROP_IN.to_string()]), "the drop-in is already there");
        assert!(!needs_oom_drop_in(&format!("{installer}OOMPolicy=kill\n"), &[]), "the unit chose a policy itself");
        assert!(!needs_oom_drop_in("[Unit]\nDescription=Someone else's daemon\n", &[]), "not ours");
        assert!(!needs_oom_drop_in(&systemd_unit(&spec("/x/kybernd", "/d")), &[]), "a login-service unit carries it");
    }
}
