//! Windows-only, narrowly scoped repair for the Codex desktop pet hit-test.
//!
//! The tests below describe the contract before the native implementation is added:
//! only the small Codex pet window may be touched, and the repair must preserve all
//! other extended-window-style flags.

#[cfg(test)]
mod tests {
    use super::{clear_transparent_exstyle, is_codex_pet_window};

    #[test]
    fn accepts_only_the_small_codex_pet_window() {
        assert!(is_codex_pet_window(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.727.6591.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
            "Codex",
            "Chrome_WidgetWin_1",
            408,
            400,
            true,
            true,
        ));
    }

    #[test]
    fn rejects_the_large_codex_main_window() {
        assert!(!is_codex_pet_window(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.727.6591.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
            "ChatGPT",
            "Chrome_WidgetWin_1",
            1550,
            830,
            true,
            true,
        ));
    }

    #[test]
    fn rejects_non_codex_windows_and_invalid_state() {
        assert!(!is_codex_pet_window(
            r"C:\Program Files\Microsoft VS Code\Code.exe",
            "Codex",
            "Chrome_WidgetWin_1",
            408,
            400,
            true,
            true,
        ));
        assert!(!is_codex_pet_window(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.727.6591.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
            "Codex",
            "Chrome_WidgetWin_1",
            408,
            400,
            false,
            true,
        ));
        assert!(!is_codex_pet_window(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.727.6591.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
            "Codex",
            "Chrome_WidgetWin_1",
            408,
            400,
            true,
            false,
        ));
    }

    #[test]
    fn clears_only_the_click_through_flag() {
        let before = 0x0280_0AA_u64;
        let after = clear_transparent_exstyle(before);
        assert_eq!(after, before & !0x20);
        assert_eq!(after & 0x08, before & 0x08); // topmost is preserved
        assert_eq!(after & 0x80, before & 0x80); // tool-window is preserved
    }

    #[test]
    fn leaves_already_interactive_windows_unchanged() {
        assert_eq!(clear_transparent_exstyle(0x0280_088), 0x0280_088);
    }
}
