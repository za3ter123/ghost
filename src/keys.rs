//! Posted keys. Printable chars go as WM_CHAR only (a posted KEYDOWN would be translated into a second
//! char); named keys as KEYDOWN/KEYUP. ctrl/alt/win combos can't be posted, because the app reads the real
//! keyboard state, except through a classic menu's accelerator.
use crate::engine::{Fail, R};
use crate::win;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, ToUnicode, VkKeyScanW, MAPVK_VK_TO_VSC};
use windows::Win32::UI::WindowsAndMessaging::*;

const VK: [(&str, u32); 34] = [
    ("enter", 0x0D), ("return", 0x0D), ("tab", 0x09), ("esc", 0x1B), ("escape", 0x1B), ("space", 0x20), ("backspace", 0x08), ("bs", 0x08),
    ("delete", 0x2E), ("del", 0x2E), ("insert", 0x2D), ("ins", 0x2D), ("home", 0x24), ("end", 0x23), ("pgup", 0x21), ("pageup", 0x21),
    ("pgdn", 0x22), ("pagedown", 0x22), ("left", 0x25), ("up", 0x26), ("right", 0x27), ("down", 0x28),
    ("f1", 0x70), ("f2", 0x71), ("f3", 0x72), ("f4", 0x73), ("f5", 0x74), ("f6", 0x75), ("f7", 0x76), ("f8", 0x77), ("f9", 0x78), ("f10", 0x79), ("f11", 0x7A),
    ("f12", 0x7B),
];

/// The character shift+c produces on the current keyboard layout (shift+a = A, shift+1 = ! on US).
fn shifted(c: char) -> char {
    let mut b = [0u16; 2];
    if c.encode_utf16(&mut b).len() != 1 {
        return c;
    }
    let sc = unsafe { VkKeyScanW(b[0]) };
    if sc == -1 {
        return c;
    }
    let vk = (sc & 0xFF) as u32;
    let mut ks = [0u8; 256];
    ks[0x10] = 0x80;
    let mut out = [0u16; 8];
    let n = unsafe { ToUnicode(vk, MapVirtualKeyW(vk, MAPVK_VK_TO_VSC), Some(&ks), &mut out, 4) }; // 4: leave keyboard state alone
    if n == 1 {
        char::from_u32(out[0] as u32).unwrap_or(c)
    } else {
        c
    }
}

fn lparam(vk: u32, up: bool) -> LPARAM {
    let mut l: i64 = 1 | ((unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) } as i64) << 16);
    if (0x21..=0x28).contains(&vk) || vk == 0x2D || vk == 0x2E {
        l |= 1 << 24; // extended keys
    }
    if up {
        l |= 0xC000_0000;
    }
    LPARAM(l as isize)
}

fn menu_accel(menu: HMENU, keys: &str, depth: u32) -> u32 {
    if menu.0.is_null() || depth > 4 {
        return 0;
    }
    unsafe {
        let n = GetMenuItemCount(Some(menu));
        for i in 0..n.max(0) {
            let sub = GetSubMenu(menu, i);
            if !sub.0.is_null() {
                let id = menu_accel(sub, keys, depth + 1);
                if id > 0 {
                    return id;
                }
                continue;
            }
            let mut b = [0u16; 256];
            let len = GetMenuStringW(menu, i as u32, Some(&mut b), MF_BYPOSITION);
            let t = String::from_utf16_lossy(&b[..len.max(0) as usize]);
            if let Some(tab) = t.find('\t') {
                if t[tab + 1..].replace(' ', "").to_lowercase().replace("control", "ctrl") == keys {
                    let id = GetMenuItemID(menu, i);
                    if id != u32::MAX {
                        return id;
                    }
                }
            }
        }
    }
    0
}

pub fn key(target: HWND, keys: &str) -> R<()> {
    let parts: Vec<&str> = if keys == "+" { vec!["+"] } else { keys.split('+').collect() };
    let (mut shift, mut combo) = (false, false);
    for m in &parts[..parts.len() - 1] {
        match m.to_lowercase().as_str() {
            "shift" => shift = true,
            "ctrl" | "control" | "alt" | "win" => combo = true,
            _ => return Err(Fail::Usage(format!("unknown modifier: {m}"))),
        }
    }
    let k = parts[parts.len() - 1];
    if k.is_empty() {
        return Err(Fail::Usage(format!("missing key name in: {keys}")));
    }
    let named = VK.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|&(_, v)| v);
    if named.is_none() && k.chars().count() != 1 {
        return Err(Fail::Usage(format!("unknown key: {k}")));
    }
    if combo {
        // Accelerators can't be posted, but a classic menu shows them ("Save\tCtrl+S"): send that item's command.
        let root = win::root_of(target);
        let id = menu_accel(unsafe { GetMenu(root) }, &keys.to_lowercase().replace("control", "ctrl"), 0);
        if id > 0 {
            unsafe {
                let _ = PostMessageW(Some(root), WM_COMMAND, WPARAM((id & 0xFFFF) as usize), LPARAM(0));
            }
            return Ok(());
        }
    }
    if combo || (shift && named.is_some()) {
        return Err(Fail::Ghost("background unavailable: modifier combos can't be sent without the real keyboard; click the menu item via tree instead".into()));
    }
    unsafe {
        if let Some(vk) = named {
            let _ = PostMessageW(Some(target), WM_KEYDOWN, WPARAM(vk as usize), lparam(vk, false));
            let _ = PostMessageW(Some(target), WM_KEYUP, WPARAM(vk as usize), lparam(vk, true));
            return Ok(());
        }
        let c = k.chars().next().unwrap();
        let ch = if shift { shifted(c) } else { c };
        let mut b = [0u16; 2];
        for u in ch.encode_utf16(&mut b) {
            let vk = (VkKeyScanW(*u) & 0xFF) as u32;
            let _ = PostMessageW(Some(target), WM_CHAR, WPARAM(*u as usize), lparam(vk, false));
        }
    }
    Ok(())
}
