//! OS integration without extra crates: desktop notifications, clipboard and opening URLs.
//! Everything here is best-effort and never blocks the UI.

use std::io::Write;
use std::process::{Command, Stdio};

fn spawn_quiet(mut cmd: Command) -> bool {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok()
}

pub fn notify(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let script = format!("display notification {} with title {}", quote(body), quote(title));
        let mut cmd = Command::new("osascript");
        cmd.arg("-e").arg(script);
        spawn_quiet(cmd);
    }
    #[cfg(windows)]
    {
        let quote = |s: &str| s.replace('\'', "''");
        let script = format!(
            "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; \
             $t = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
             $x = $t.GetElementsByTagName('text'); \
             $x.Item(0).AppendChild($t.CreateTextNode('{}')) > $null; \
             $x.Item(1).AppendChild($t.CreateTextNode('{}')) > $null; \
             [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show([Windows.UI.Notifications.ToastNotification]::new($t))",
            quote(title),
            quote(body)
        );
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script]);
        spawn_quiet(cmd);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut cmd = Command::new("notify-send");
        cmd.args(["--app-name=gcli", title, body]);
        spawn_quiet(cmd);
    }
}

/// Opens `url` in the browser. Only http(s) URLs with a host: links can come from Google Cloud
/// (a service's URL, say), and the system opener would also run files, `file:` paths and
/// other schemes.
pub fn open_url(url: &str) -> bool {
    if !is_web_url(url) {
        return false;
    }
    #[cfg(target_os = "macos")]
    let cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(windows)]
    let cmd = {
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    spawn_quiet(cmd)
}

pub fn is_web_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")) else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty() && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn pipe_to(program: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut child) =
        Command::new(program).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
    else {
        return false;
    };
    let written = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    written && child.wait().is_ok_and(|s| s.success())
}

/// Copies to the system clipboard; falls back to the OSC 52 escape sequence,
/// which most modern terminals (and SSH sessions) understand.
pub fn copy_to_clipboard(text: &str) -> bool {
    // Pasted into a terminal, escape sequences or invisible characters would act or hide.
    let text = &crate::util::sanitize(text);
    let native = if cfg!(target_os = "macos") {
        pipe_to("pbcopy", &[], text)
    } else if cfg!(windows) {
        // Read stdin as UTF-8, not the console code page, so non-ASCII text survives.
        pipe_to(
            "powershell",
            &["-NoProfile", "-Command", "[Console]::InputEncoding = [Text.Encoding]::UTF8; $input | Set-Clipboard"],
            text,
        )
    } else {
        pipe_to("wl-copy", &[], text)
            || pipe_to("xclip", &["-selection", "clipboard"], text)
            || pipe_to("xsel", &["--clipboard", "--input"], text)
    };
    if native {
        return true;
    }
    let mut out = std::io::stdout();
    write!(out, "\u{1b}]52;c;{}\u{7}", crate::util::base64(text.as_bytes())).is_ok() && out.flush().is_ok()
}
