//! Zero-extra-syscall Microsoft Edge WebView2 (`msedgewebview2.exe`) classifier
//! and host application attribution parser.
//!
//! Operates strictly on in-memory `ProcessColdMeta::command_line` strings already
//! cached by the slow lane — performs zero OS calls.

use loom_types::WebView2Role;

/// Borrowed classification result for a `msedgewebview2.exe` process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebView2Parsed<'a> {
    /// Identified Chromium / WebView2 subprocess role.
    pub role: WebView2Role,
    /// Host executable filename extracted from `--webview-exe-name=<host.exe>`, if present.
    pub host_exe_name: Option<&'a str>,
}

/// Returns `true` if `image_name` refers to the Edge WebView2 runtime (`msedgewebview2.exe`).
#[inline]
pub fn is_webview2_image(image_name: &str) -> bool {
    image_name.eq_ignore_ascii_case("msedgewebview2.exe")
}

/// Classifies a `msedgewebview2.exe` process purely from its cached command line.
///
/// Returns `None` if `image_name` is not `msedgewebview2.exe` or if `command_line`
/// has not yet been resolved on the slow lane.
pub fn classify_webview2<'a>(
    image_name: &str,
    command_line: Option<&'a str>,
) -> Option<WebView2Parsed<'a>> {
    if !is_webview2_image(image_name) {
        return None;
    }
    let cmd = command_line?;

    let host_exe_name = extract_flag_value(cmd, "--webview-exe-name=").map(extract_basename);
    let proc_type = extract_flag_value(cmd, "--type=");
    let role = match proc_type {
        None | Some("") => WebView2Role::Browser,
        Some(t) if t.eq_ignore_ascii_case("renderer") => WebView2Role::Renderer,
        Some(t) if t.eq_ignore_ascii_case("gpu-process") => WebView2Role::Gpu,
        Some(t) if t.eq_ignore_ascii_case("crashpad-handler") => WebView2Role::Crashpad,
        Some(t) if t.eq_ignore_ascii_case("utility") => {
            classify_utility_sub_type(extract_flag_value(cmd, "--utility-sub-type="))
        }
        Some(_) => WebView2Role::Utility,
    };

    Some(WebView2Parsed {
        role,
        host_exe_name,
    })
}

fn classify_utility_sub_type(sub_type: Option<&str>) -> WebView2Role {
    let Some(sub) = sub_type else {
        return WebView2Role::Utility;
    };
    let lower = sub.to_ascii_lowercase();
    if lower.contains("network") {
        WebView2Role::Network
    } else if lower.contains("storage") {
        WebView2Role::Storage
    } else if lower.contains("audio") {
        WebView2Role::Audio
    } else {
        WebView2Role::Utility
    }
}

/// Extracts the value immediately following a Chromium `--flag=` switch from a Windows
/// command line string, supporting both unquoted and double-quoted values.
fn extract_flag_value<'a>(cmd: &'a str, flag_prefix: &str) -> Option<&'a str> {
    let bytes = cmd.as_bytes();
    let prefix_bytes = flag_prefix.as_bytes();
    if bytes.len() < prefix_bytes.len() {
        return None;
    }

    let mut pos = 0;
    while pos + prefix_bytes.len() <= bytes.len() {
        let at_boundary =
            pos == 0 || bytes[pos - 1].is_ascii_whitespace() || bytes[pos - 1] == b'"';
        if at_boundary && bytes[pos..pos + prefix_bytes.len()].eq_ignore_ascii_case(prefix_bytes) {
            let val_start = pos + prefix_bytes.len();
            let rest = &cmd[val_start..];
            if let Some(after_quote) = rest.strip_prefix('"') {
                let end = after_quote.find('"').unwrap_or(after_quote.len());
                let val = after_quote[..end].trim();
                return (!val.is_empty()).then_some(val);
            }
            let end = rest
                .find(|c: char| c.is_ascii_whitespace() || c == '"')
                .unwrap_or(rest.len());
            let val = rest[..end].trim();
            return (!val.is_empty()).then_some(val);
        }
        pos += 1;
    }
    None
}

/// Extracts the trailing executable filename if `raw` includes directory separators.
#[inline]
fn extract_basename(raw: &str) -> &str {
    raw.rsplit(['\\', '/']).next().unwrap_or(raw).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_webview2_or_missing_cmdline_returns_none() {
        assert_eq!(
            classify_webview2("chrome.exe", Some("--type=renderer")),
            None
        );
        assert_eq!(classify_webview2("msedgewebview2.exe", None), None);
    }

    #[test]
    fn classifies_all_webview2_roles_and_host_names() {
        let browser = classify_webview2(
            "MSEdgeWebView2.exe",
            Some(r#""C:\Program Files\WebView2\msedgewebview2.exe" --embedded-browser-webview=1 --webview-exe-name=Teams.exe"#),
        )
        .unwrap();
        assert_eq!(browser.role, WebView2Role::Browser);
        assert_eq!(browser.host_exe_name, Some("Teams.exe"));

        let renderer = classify_webview2(
            "msedgewebview2.exe",
            Some(r#"msedgewebview2.exe --type=renderer --webview-exe-name="My Cool App.exe" --lang=en-US"#),
        )
        .unwrap();
        assert_eq!(renderer.role, WebView2Role::Renderer);
        assert_eq!(renderer.host_exe_name, Some("My Cool App.exe"));

        let gpu = classify_webview2(
            "msedgewebview2.exe",
            Some(r#"msedgewebview2.exe --type=gpu-process --webview-exe-name=C:\Apps\Outlook.exe"#),
        )
        .unwrap();
        assert_eq!(gpu.role, WebView2Role::Gpu);
        assert_eq!(gpu.host_exe_name, Some("Outlook.exe"));

        let net = classify_webview2(
            "msedgewebview2.exe",
            Some("--type=utility --utility-sub-type=network.mojom.NetworkService"),
        )
        .unwrap();
        assert_eq!(net.role, WebView2Role::Network);

        let storage = classify_webview2(
            "msedgewebview2.exe",
            Some("--type=utility --utility-sub-type=storage.mojom.StorageService"),
        )
        .unwrap();
        assert_eq!(storage.role, WebView2Role::Storage);

        let audio = classify_webview2(
            "msedgewebview2.exe",
            Some("--type=utility --utility-sub-type=audio.mojom.AudioService"),
        )
        .unwrap();
        assert_eq!(audio.role, WebView2Role::Audio);

        let utility = classify_webview2(
            "msedgewebview2.exe",
            Some("--type=utility --utility-sub-type=data_decoder.mojom.DataDecoderService"),
        )
        .unwrap();
        assert_eq!(utility.role, WebView2Role::Utility);

        let crashpad = classify_webview2(
            "msedgewebview2.exe",
            Some("--type=crashpad-handler --webview-exe-name=Widgets.exe"),
        )
        .unwrap();
        assert_eq!(crashpad.role, WebView2Role::Crashpad);
        assert_eq!(crashpad.host_exe_name, Some("Widgets.exe"));
    }
}
