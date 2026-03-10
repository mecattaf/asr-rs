use anyhow::{Context, Result};
use std::io::Write;
use std::process::Command;

pub trait TextInjector: Send + Sync {
    fn inject(&self, text: &str) -> Result<()>;
    fn is_available(&self) -> bool;
    fn name(&self) -> &'static str;
}

/// Checks whether a binary is on $PATH.
fn which(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Injects text via wtype (wlroots compositors).
pub struct WtypeInjector;

impl TextInjector for WtypeInjector {
    fn inject(&self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let status = Command::new("wtype")
            .arg("--")
            .arg(text)
            .status()
            .context("failed to run wtype")?;
        if !status.success() {
            anyhow::bail!("wtype exited with {status}");
        }
        Ok(())
    }

    fn is_available(&self) -> bool {
        which("wtype")
    }

    fn name(&self) -> &'static str {
        "wtype"
    }
}

/// Injects text via dotool's persistent Unix socket (dotoolc).
pub struct DotoolInjector;

impl TextInjector for DotoolInjector {
    fn inject(&self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let mut child = Command::new("dotoolc")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("failed to run dotoolc")?;
        if let Some(ref mut stdin) = child.stdin {
            write!(stdin, "type {text}")?;
        }
        let status = child.wait()?;
        if !status.success() {
            anyhow::bail!("dotoolc exited with {status}");
        }
        Ok(())
    }

    fn is_available(&self) -> bool {
        which("dotoolc")
    }

    fn name(&self) -> &'static str {
        "dotool"
    }
}

/// Copies text to the Wayland clipboard via wl-copy. Universal fallback.
pub struct ClipboardInjector;

impl TextInjector for ClipboardInjector {
    fn inject(&self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let mut child = Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("failed to run wl-copy")?;
        if let Some(ref mut stdin) = child.stdin {
            write!(stdin, "{text}")?;
        }
        let status = child.wait()?;
        if !status.success() {
            anyhow::bail!("wl-copy exited with {status}");
        }
        Ok(())
    }

    fn is_available(&self) -> bool {
        which("wl-copy")
    }

    fn name(&self) -> &'static str {
        "clipboard"
    }
}

/// Copies text to clipboard via wl-copy, then simulates a paste keystroke via wtype.
/// Safe for terminals (triggers bracketed paste in kitty/nvim).
pub struct PasteInjector {
    /// Parsed modifier keys (e.g., ["ctrl", "shift"]).
    modifiers: Vec<String>,
    /// The final key (e.g., "v").
    key: String,
}

impl PasteInjector {
    pub fn new(paste_keys: &str) -> Self {
        let parts: Vec<&str> = paste_keys.split('+').collect();
        let key = parts.last().unwrap_or(&"v").to_string();
        let modifiers = parts[..parts.len().saturating_sub(1)]
            .iter()
            .map(|s| s.to_string())
            .collect();
        Self { modifiers, key }
    }
}

impl TextInjector for PasteInjector {
    fn inject(&self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        // Copy to clipboard
        let mut child = Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("failed to run wl-copy")?;
        if let Some(ref mut stdin) = child.stdin {
            write!(stdin, "{text}")?;
        }
        let status = child.wait()?;
        if !status.success() {
            anyhow::bail!("wl-copy exited with {status}");
        }

        // Simulate paste keystroke via wtype
        let mut args = Vec::new();
        for m in &self.modifiers {
            args.push("-M".to_string());
            args.push(m.clone());
        }
        args.push("-k".to_string());
        args.push(self.key.clone());
        for m in self.modifiers.iter().rev() {
            args.push("-m".to_string());
            args.push(m.clone());
        }

        let status = Command::new("wtype")
            .args(&args)
            .status()
            .context("failed to run wtype for paste")?;
        if !status.success() {
            anyhow::bail!("wtype paste exited with {status}");
        }
        Ok(())
    }

    fn is_available(&self) -> bool {
        which("wl-copy") && which("wtype")
    }

    fn name(&self) -> &'static str {
        "paste"
    }
}

/// Tries multiple injectors in order, falling back on failure or unavailability.
pub struct InjectorChain {
    drivers: Vec<Box<dyn TextInjector>>,
}

impl InjectorChain {
    pub fn new(drivers: Vec<Box<dyn TextInjector>>) -> Self {
        Self { drivers }
    }
}

impl TextInjector for InjectorChain {
    fn inject(&self, text: &str) -> Result<()> {
        for driver in &self.drivers {
            if !driver.is_available() {
                tracing::debug!("{} not available, skipping", driver.name());
                continue;
            }
            match driver.inject(text) {
                Ok(()) => return Ok(()),
                Err(e) => tracing::warn!("{} failed: {e}, trying next", driver.name()),
            }
        }
        anyhow::bail!("all injection methods failed")
    }

    fn is_available(&self) -> bool {
        self.drivers.iter().any(|d| d.is_available())
    }

    fn name(&self) -> &'static str {
        "chain"
    }
}

/// Queries Niri IPC to detect terminal focus, switching between default and terminal chains.
pub struct NiriAwareInjector {
    default_chain: InjectorChain,
    terminal_chain: InjectorChain,
    terminal_app_ids: Vec<String>,
}

impl NiriAwareInjector {
    pub fn new(
        default_chain: InjectorChain,
        terminal_chain: InjectorChain,
        terminal_app_ids: Vec<String>,
    ) -> Self {
        Self {
            default_chain,
            terminal_chain,
            terminal_app_ids,
        }
    }

    fn is_terminal_focused(&self) -> bool {
        let output = Command::new("niri")
            .args(["msg", "-j", "focused-window"])
            .output();
        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return false,
        };
        let json: serde_json::Value = match serde_json::from_slice(&output.stdout) {
            Ok(v) => v,
            Err(_) => return false,
        };
        if let Some(app_id) = json.get("app_id").and_then(|v| v.as_str()) {
            self.terminal_app_ids.iter().any(|t| t == app_id)
        } else {
            false
        }
    }
}

impl TextInjector for NiriAwareInjector {
    fn inject(&self, text: &str) -> Result<()> {
        if self.is_terminal_focused() {
            tracing::debug!("terminal detected, using paste chain");
            self.terminal_chain.inject(text)
        } else {
            self.default_chain.inject(text)
        }
    }

    fn is_available(&self) -> bool {
        self.default_chain.is_available() || self.terminal_chain.is_available()
    }

    fn name(&self) -> &'static str {
        "niri-aware"
    }
}

/// Sanitize text before injection: replace line-terminating characters with space
/// to prevent unintended Enter keypresses that can submit forms mid-sentence.
pub fn sanitize_for_injection(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\r' | '\x0B' | '\x0C' | '\u{0085}' | '\u{2028}' | '\u{2029}' => ' ',
            c => c,
        })
        .collect()
}

/// Build a single injector from a driver name.
fn make_driver(name: &str, paste_keys: &str) -> Option<Box<dyn TextInjector>> {
    match name {
        "wtype" => Some(Box::new(WtypeInjector)),
        "dotool" => Some(Box::new(DotoolInjector)),
        "clipboard" => Some(Box::new(ClipboardInjector)),
        "paste" => Some(Box::new(PasteInjector::new(paste_keys))),
        _ => {
            tracing::warn!("unknown injection driver: {name}");
            None
        }
    }
}

/// Build an InjectorChain from a list of driver names.
pub fn build_chain(driver_names: &[String], paste_keys: &str) -> InjectorChain {
    let drivers: Vec<Box<dyn TextInjector>> = driver_names
        .iter()
        .filter_map(|name| make_driver(name, paste_keys))
        .collect();
    InjectorChain::new(drivers)
}

/// Create the full injector based on config.
/// If niri_detect is enabled, wraps in NiriAwareInjector.
pub fn create_injector(config: &crate::config::InjectionConfig) -> Box<dyn TextInjector> {
    let default_chain = build_chain(&config.driver_order, &config.paste_keys);

    if config.niri_detect {
        let terminal_chain = build_chain(&["paste".into(), "clipboard".into()], &config.paste_keys);
        Box::new(NiriAwareInjector::new(
            default_chain,
            terminal_chain,
            config.terminal_app_ids.clone(),
        ))
    } else {
        Box::new(default_chain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Mock injector that tracks calls and can be configured to fail.
    struct MockInjector {
        name: &'static str,
        available: bool,
        should_fail: bool,
        call_count: Arc<AtomicUsize>,
    }

    impl TextInjector for MockInjector {
        fn inject(&self, _text: &str) -> Result<()> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if self.should_fail {
                anyhow::bail!("{} mock failure", self.name)
            } else {
                Ok(())
            }
        }

        fn is_available(&self) -> bool {
            self.available
        }

        fn name(&self) -> &'static str {
            self.name
        }
    }

    #[test]
    fn chain_uses_first_available() {
        let c1 = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::new(AtomicUsize::new(0));
        let chain = InjectorChain::new(vec![
            Box::new(MockInjector {
                name: "first",
                available: true,
                should_fail: false,
                call_count: c1.clone(),
            }),
            Box::new(MockInjector {
                name: "second",
                available: true,
                should_fail: false,
                call_count: c2.clone(),
            }),
        ]);
        chain.inject("hello").unwrap();
        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn chain_skips_unavailable() {
        let c1 = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::new(AtomicUsize::new(0));
        let chain = InjectorChain::new(vec![
            Box::new(MockInjector {
                name: "first",
                available: false,
                should_fail: false,
                call_count: c1.clone(),
            }),
            Box::new(MockInjector {
                name: "second",
                available: true,
                should_fail: false,
                call_count: c2.clone(),
            }),
        ]);
        chain.inject("hello").unwrap();
        assert_eq!(c1.load(Ordering::SeqCst), 0);
        assert_eq!(c2.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn chain_falls_back_on_failure() {
        let c1 = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::new(AtomicUsize::new(0));
        let chain = InjectorChain::new(vec![
            Box::new(MockInjector {
                name: "first",
                available: true,
                should_fail: true,
                call_count: c1.clone(),
            }),
            Box::new(MockInjector {
                name: "second",
                available: true,
                should_fail: false,
                call_count: c2.clone(),
            }),
        ]);
        chain.inject("hello").unwrap();
        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn chain_all_fail_returns_error() {
        let c1 = Arc::new(AtomicUsize::new(0));
        let chain = InjectorChain::new(vec![Box::new(MockInjector {
            name: "only",
            available: true,
            should_fail: true,
            call_count: c1,
        })]);
        assert!(chain.inject("hello").is_err());
    }

    #[test]
    fn chain_is_available_any() {
        let chain = InjectorChain::new(vec![
            Box::new(MockInjector {
                name: "a",
                available: false,
                should_fail: false,
                call_count: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(MockInjector {
                name: "b",
                available: true,
                should_fail: false,
                call_count: Arc::new(AtomicUsize::new(0)),
            }),
        ]);
        assert!(chain.is_available());
    }

    #[test]
    fn paste_injector_parses_keys() {
        let p = PasteInjector::new("ctrl+shift+v");
        assert_eq!(p.modifiers, vec!["ctrl", "shift"]);
        assert_eq!(p.key, "v");
    }

    #[test]
    fn paste_injector_single_key() {
        let p = PasteInjector::new("v");
        assert!(p.modifiers.is_empty());
        assert_eq!(p.key, "v");
    }

    #[test]
    fn niri_ipc_json_parsing() {
        // Test the JSON parsing logic directly
        let json = r#"{"id":12,"title":"~","app_id":"kitty","workspace_id":6,"is_focused":true}"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let app_id = v.get("app_id").and_then(|v| v.as_str()).unwrap();
        assert_eq!(app_id, "kitty");

        let terminals = vec!["kitty".to_string(), "foot".to_string()];
        assert!(terminals.iter().any(|t| t == app_id));
    }

    #[test]
    fn niri_ipc_non_terminal() {
        let json = r#"{"id":5,"title":"Firefox","app_id":"firefox","workspace_id":1,"is_focused":true}"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let app_id = v.get("app_id").and_then(|v| v.as_str()).unwrap();

        let terminals = vec!["kitty".to_string(), "foot".to_string()];
        assert!(!terminals.iter().any(|t| t == app_id));
    }

    #[test]
    fn niri_ipc_null_response() {
        // niri returns "null" when no window is focused
        let json = "null";
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let app_id = v.get("app_id").and_then(|v| v.as_str());
        assert!(app_id.is_none());
    }
}
