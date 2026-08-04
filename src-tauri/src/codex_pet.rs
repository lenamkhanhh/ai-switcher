//! Windows-only, narrowly scoped repair for the Codex desktop pet hit-test.
//!
//! Codex renders the pet in a separate transparent top-level window. On the affected
//! Windows build that window also carries `WS_EX_TRANSPARENT`, so Windows sends pointer
//! hit-tests to the Codex main window underneath it. We remove only that one style bit
//! from the small pet window; account/profile/auth state is not involved.

const WS_EX_TRANSPARENT: u64 = 0x20;
const PET_MIN_DIMENSION: i32 = 120;
const PET_MAX_DIMENSION: i32 = 700;
const PET_SAFE_EDGE_MARGIN: i32 = 20;
// The pet's activity bubble is laid out above/below the sprite depending on its screen position.
// Keeping this much room below the overlay prevents Codex from putting the sprite and controls
// past the bottom of its fixed-size transparent window.
const PET_SAFE_BOTTOM_MARGIN: i32 = 180;

pub(crate) fn is_codex_pet_window(
    process_path: &str,
    title: &str,
    class_name: &str,
    width: i32,
    height: i32,
    visible: bool,
    enabled: bool,
) -> bool {
    let process = process_path.to_ascii_lowercase();
    process.contains("openai.codex_")
        && process.ends_with("\\chatgpt.exe")
        && title.eq_ignore_ascii_case("codex")
        && class_name == "Chrome_WidgetWin_1"
        && (PET_MIN_DIMENSION..=PET_MAX_DIMENSION).contains(&width)
        && (PET_MIN_DIMENSION..=PET_MAX_DIMENSION).contains(&height)
        && visible
        && enabled
}

pub(crate) fn clear_transparent_exstyle(exstyle: u64) -> u64 {
    exstyle & !WS_EX_TRANSPARENT
}

pub(crate) fn safe_pet_position(
    current: (i32, i32, i32, i32),
    work_area: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (left, top, width, height) = current;
    let (work_left, work_top, work_right, work_bottom) = work_area;
    let min_x = work_left + PET_SAFE_EDGE_MARGIN;
    let max_x = (work_right - width - PET_SAFE_EDGE_MARGIN).max(min_x);
    let min_y = work_top + PET_SAFE_EDGE_MARGIN;
    let max_y = (work_bottom - height - PET_SAFE_BOTTOM_MARGIN).max(min_y);
    (left.clamp(min_x, max_x), top.clamp(min_y, max_y))
}

pub(crate) fn should_reposition_pet(
    current: (i32, i32, i32, i32),
    work_area: (i32, i32, i32, i32),
) -> bool {
    let target = safe_pet_position(current, work_area);
    target != (current.0, current.1)
}

#[cfg(windows)]
mod windows_impl {
    use super::{clear_transparent_exstyle, is_codex_pet_window};
    use std::ffi::c_void;
    use std::mem::MaybeUninit;
    use std::thread;
    use std::time::Duration;

    type Hwnd = isize;
    type Handle = *mut c_void;
    type Bool = i32;
    type Dword = u32;
    type LongPtr = isize;

    const TRUE: Bool = 1;
    const FALSE: Bool = 0;
    const GWL_EXSTYLE: i32 = -20;
    const PROCESS_QUERY_LIMITED_INFORMATION: Dword = 0x1000;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_FRAMECHANGED: u32 = 0x0020;
    const MONITOR_DEFAULTTONEAREST: u32 = 0x00000002;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct MonitorInfo {
        cb_size: Dword,
        monitor: Rect,
        work: Rect,
        flags: Dword,
    }

    type EnumWindowsProc = unsafe extern "system" fn(Hwnd, isize) -> Bool;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(callback: EnumWindowsProc, lparam: isize) -> Bool;
        fn GetWindowTextLengthW(hwnd: Hwnd) -> i32;
        fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max_count: i32) -> i32;
        fn GetClassNameW(hwnd: Hwnd, class_name: *mut u16, max_count: i32) -> i32;
        fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut Dword) -> Dword;
        fn GetWindowRect(hwnd: Hwnd, rect: *mut Rect) -> Bool;
        fn MonitorFromWindow(hwnd: Hwnd, flags: Dword) -> Hwnd;
        fn GetMonitorInfoW(monitor: Hwnd, info: *mut MonitorInfo) -> Bool;
        fn GetWindowLongPtrW(hwnd: Hwnd, index: i32) -> LongPtr;
        fn SetWindowLongPtrW(hwnd: Hwnd, index: i32, value: LongPtr) -> LongPtr;
        fn IsWindowVisible(hwnd: Hwnd) -> Bool;
        fn IsWindowEnabled(hwnd: Hwnd) -> Bool;
        fn SetWindowPos(
            hwnd: Hwnd,
            insert_after: Hwnd,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            flags: u32,
        ) -> Bool;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: Dword, inherit_handle: Bool, process_id: Dword) -> Handle;
        fn QueryFullProcessImageNameW(
            process: Handle,
            flags: Dword,
            filename: *mut u16,
            size: *mut Dword,
        ) -> Bool;
        fn CloseHandle(handle: Handle) -> Bool;
    }

    struct RepairContext {
        repaired: usize,
    }

    fn read_window_text(hwnd: Hwnd) -> String {
        // Electron/Tauri titles are short, but cap the allocation so a malformed window
        // cannot cause an unbounded read during the periodic repair scan.
        let length = unsafe { GetWindowTextLengthW(hwnd) }.clamp(0, 512) as usize;
        let mut buffer = vec![0u16; length.saturating_add(1)];
        let count = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
        String::from_utf16_lossy(&buffer[..(count.max(0) as usize).min(buffer.len())])
    }

    fn read_class_name(hwnd: Hwnd) -> String {
        let mut buffer = vec![0u16; 256];
        let count = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
        String::from_utf16_lossy(&buffer[..(count.max(0) as usize).min(buffer.len())])
    }

    fn process_path(process_id: Dword) -> Option<String> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, process_id) };
        if handle.is_null() {
            return None;
        }

        let mut buffer = vec![0u16; 1024];
        let mut length = buffer.len() as Dword;
        let succeeded = unsafe {
            QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) == TRUE
        };
        unsafe {
            CloseHandle(handle);
        }
        succeeded.then(|| String::from_utf16_lossy(&buffer[..length as usize]))
    }

    fn monitor_work_area(hwnd: Hwnd) -> Option<(i32, i32, i32, i32)> {
        let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        if monitor == 0 {
            return None;
        }
        let mut info = MonitorInfo {
            cb_size: std::mem::size_of::<MonitorInfo>() as Dword,
            monitor: Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            work: Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            flags: 0,
        };
        (unsafe { GetMonitorInfoW(monitor, &mut info) } == TRUE).then_some((
            info.work.left,
            info.work.top,
            info.work.right,
            info.work.bottom,
        ))
    }

    unsafe extern "system" fn enum_windows_proc(hwnd: Hwnd, lparam: isize) -> Bool {
        let context = &mut *(lparam as *mut RepairContext);
        if IsWindowVisible(hwnd) == FALSE || IsWindowEnabled(hwnd) == FALSE {
            return TRUE;
        }

        let title = read_window_text(hwnd);
        let class_name = read_class_name(hwnd);
        if !title.eq_ignore_ascii_case("codex") || class_name != "Chrome_WidgetWin_1" {
            return TRUE;
        }

        let mut rect = MaybeUninit::<Rect>::uninit();
        if GetWindowRect(hwnd, rect.as_mut_ptr()) == FALSE {
            return TRUE;
        }
        let rect = rect.assume_init();
        let width = rect.right.saturating_sub(rect.left);
        let height = rect.bottom.saturating_sub(rect.top);
        let mut process_id = 0;
        GetWindowThreadProcessId(hwnd, &mut process_id);
        let Some(path) = process_path(process_id) else {
            return TRUE;
        };
        if !is_codex_pet_window(&path, &title, &class_name, width, height, true, true) {
            return TRUE;
        }

        let before = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as usize as u64;
        let after = clear_transparent_exstyle(before);
        let current = (rect.left, rect.top, width, height);
        let work_area = monitor_work_area(hwnd);
        let target = work_area
            .map(|work| super::safe_pet_position(current, work))
            .unwrap_or((rect.left, rect.top));
        let style_changed = before != after;
        let position_changed = work_area
            .map(|work| super::should_reposition_pet(current, work))
            .unwrap_or(false);
        if !style_changed && !position_changed {
            return TRUE;
        }

        // Keep position, z-order, activation, topmost, and every other style bit intact.
        if style_changed {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, after as LongPtr);
        }
        SetWindowPos(
            hwnd,
            0,
            target.0,
            target.1,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as usize as u64;
        let mut position_ok = true;
        if position_changed {
            let mut moved = MaybeUninit::<Rect>::uninit();
            if GetWindowRect(hwnd, moved.as_mut_ptr()) == FALSE {
                position_ok = false;
            } else {
                let moved = moved.assume_init();
                position_ok = moved.left == target.0 && moved.top == target.1;
            }
        }
        if current == after && position_ok {
            context.repaired = context.repaired.saturating_add(1);
        }
        TRUE
    }

    pub(crate) fn repair_codex_pet_input() -> usize {
        let mut context = RepairContext { repaired: 0 };
        unsafe {
            EnumWindows(enum_windows_proc, &mut context as *mut _ as isize);
        }
        context.repaired
    }

    pub(crate) fn schedule_repair_after_codex_restart() {
        thread::spawn(|| {
            // The frontend refresh intentionally restarts Codex asynchronously. Retry for a
            // short bounded window so the new pet is repaired without a permanent process hook.
            for _ in 0..20 {
                if repair_codex_pet_input() > 0 {
                    return;
                }
                thread::sleep(Duration::from_millis(500));
            }
        });
    }
}

#[cfg(windows)]
pub(crate) use windows_impl::{repair_codex_pet_input, schedule_repair_after_codex_restart};

#[cfg(not(windows))]
pub(crate) fn repair_codex_pet_input() -> usize {
    0
}

#[cfg(not(windows))]
pub(crate) fn schedule_repair_after_codex_restart() {}

#[cfg(test)]
mod tests {
    use super::{
        clear_transparent_exstyle, is_codex_pet_window, safe_pet_position, should_reposition_pet,
    };

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

    #[test]
    fn moves_an_edge_clipped_pet_into_the_safe_work_area() {
        let current = (-12, 384, 408, 400);
        let work_area = (0, 0, 1550, 830);
        assert!(should_reposition_pet(current, work_area));
        assert_eq!(safe_pet_position(current, work_area), (20, 250));
    }

    #[test]
    fn preserves_a_pet_that_is_already_clear_of_the_edges() {
        let current = (420, 180, 408, 400);
        let work_area = (0, 0, 1550, 830);
        assert!(!should_reposition_pet(current, work_area));
        assert_eq!(safe_pet_position(current, work_area), (420, 180));
    }
}
