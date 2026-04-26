use crate::api::types::{ApiError, RemotePointerEventDto};

pub const MSG_POINTER: u8 = 1;
pub const MSG_KEY: u8 = 2;

pub const MOD_SHIFT: i32 = 1;
pub const MOD_CTRL: i32 = 2;
pub const MOD_ALT: i32 = 4;
pub const MOD_META: i32 = 8;
const MOD_MASK: i32 = MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_META;

pub const RD_KEY_CONTROL: i32 = -113;
pub const RD_KEY_SHIFT: i32 = -114;
pub const RD_KEY_ALT: i32 = -115;
pub const RD_KEY_META: i32 = -116;

fn pointer_kind_byte(kind: &str) -> u8 {
    match kind {
        "down" => 1,
        "up" => 2,
        "scroll" => 3,
        _ => 0,
    }
}

pub fn encode_pointer_msg(e: &RemotePointerEventDto) -> Vec<u8> {
    let k = pointer_kind_byte(&e.kind);
    let mut v = Vec::with_capacity(34);
    v.push(MSG_POINTER);
    v.push(k);
    v.extend_from_slice(&e.x.to_le_bytes());
    v.extend_from_slice(&e.y.to_le_bytes());
    v.extend_from_slice(&e.button.to_le_bytes());
    v.extend_from_slice(&e.delta.to_le_bytes());
    v.extend_from_slice(&e.modifiers.to_le_bytes());
    v
}

pub fn encode_key_msg(key_code: i32, down: bool, modifiers: i32) -> Vec<u8> {
    let mut v = Vec::with_capacity(14);
    v.push(MSG_KEY);
    v.extend_from_slice(&key_code.to_le_bytes());
    v.push(if down { 1 } else { 0 });
    v.extend_from_slice(&modifiers.to_le_bytes());
    v
}

pub struct HostInputProcessor {
    fw: f64,
    fh: f64,
    sw: i32,
    sh: i32,
    mod_state: i32,
    injector: HostInjector,
}

impl HostInputProcessor {
    pub fn new(fw: f64, fh: f64, sw: i32, sh: i32) -> Result<Self, ApiError> {
        Ok(Self {
            fw,
            fh,
            sw,
            sh,
            mod_state: 0,
            injector: HostInjector::new()?,
        })
    }

    pub fn dispatch_framed_payload(&mut self, buf: &[u8]) -> Result<(), ApiError> {
        if buf.is_empty() {
            return Ok(());
        }
        match buf[0] {
            MSG_POINTER if buf.len() >= 30 => {
                let kind = buf[1];
                let x = f64::from_le_bytes(buf[2..10].try_into().unwrap());
                let y = f64::from_le_bytes(buf[10..18].try_into().unwrap());
                let button = i32::from_le_bytes(buf[18..22].try_into().unwrap());
                let delta = f64::from_le_bytes(buf[22..30].try_into().unwrap());
                let modifiers = if buf.len() >= 34 {
                    i32::from_le_bytes(buf[30..34].try_into().unwrap())
                } else {
                    0
                };
                self.apply_pointer(kind, x, y, button, delta, modifiers)
            }
            MSG_KEY if buf.len() >= 10 => {
                let key_code = i32::from_le_bytes(buf[1..5].try_into().unwrap());
                let down = buf[5] != 0;
                let modifiers = i32::from_le_bytes(buf[6..10].try_into().unwrap());
                self.apply_key(key_code, down, modifiers)
            }
            _ => Ok(()),
        }
    }

    fn sync_modifiers_from_mask(&mut self, target: i32) -> Result<(), ApiError> {
        let target = target & MOD_MASK;
        let current = self.mod_state & MOD_MASK;
        if current == target {
            return Ok(());
        }
        for bit in [MOD_META, MOD_ALT, MOD_CTRL, MOD_SHIFT] {
            if (current & bit) != 0 && (target & bit) == 0 {
                self.injector.send_modifier(bit, false)?;
            }
        }
        for bit in [MOD_CTRL, MOD_SHIFT, MOD_ALT, MOD_META] {
            if (target & bit) != 0 && (current & bit) == 0 {
                self.injector.send_modifier(bit, true)?;
            }
        }
        self.mod_state = (self.mod_state & !MOD_MASK) | target;
        Ok(())
    }

    fn apply_modifier_key_event(&mut self, key_code: i32, down: bool) -> Result<(), ApiError> {
        let bit = match key_code {
            RD_KEY_CONTROL => MOD_CTRL,
            RD_KEY_SHIFT => MOD_SHIFT,
            RD_KEY_ALT => MOD_ALT,
            RD_KEY_META => MOD_META,
            _ => return Ok(()),
        };
        self.injector.send_modifier(bit, down)?;
        if down {
            self.mod_state |= bit;
        } else {
            self.mod_state &= !bit;
        }
        Ok(())
    }

    fn apply_pointer(
        &mut self,
        kind: u8,
        x: f64,
        y: f64,
        button: i32,
        delta: f64,
        modifiers: i32,
    ) -> Result<(), ApiError> {
        self.sync_modifiers_from_mask(modifiers)?;
        let (mx, my) = map_pointer_to_screen(x, y, self.fw, self.fh, self.sw, self.sh);
        self.injector.send_mouse(kind, mx, my, button, delta)
    }

    fn apply_key(&mut self, key_code: i32, down: bool, modifiers: i32) -> Result<(), ApiError> {
        if matches!(
            key_code,
            RD_KEY_CONTROL | RD_KEY_SHIFT | RD_KEY_ALT | RD_KEY_META
        ) {
            return self.apply_modifier_key_event(key_code, down);
        }
        self.sync_modifiers_from_mask(modifiers)?;
        self.injector.send_key(key_code, down, modifiers)
    }
}

fn map_pointer_to_screen(x: f64, y: f64, fw: f64, fh: f64, sw: i32, sh: i32) -> (i32, i32) {
    if fw <= 0.0 || fh <= 0.0 {
        return (0, 0);
    }
    let sx = (x / fw * sw as f64).round() as i32;
    let sy = (y / fh * sh as f64).round() as i32;
    (
        sx.clamp(0, sw.saturating_sub(1).max(0)),
        sy.clamp(0, sh.saturating_sub(1).max(0)),
    )
}

enum HostInjector {
    #[cfg(target_os = "windows")]
    Windows(WindowsSendInputInjector),
    #[cfg(not(target_os = "windows"))]
    Enigo(EnigoInjector),
}

impl HostInjector {
    fn new() -> Result<Self, ApiError> {
        #[cfg(target_os = "windows")]
        {
            Ok(Self::Windows(WindowsSendInputInjector::new()))
        }
        #[cfg(not(target_os = "windows"))]
        {
            Ok(Self::Enigo(EnigoInjector::new()?))
        }
    }

    fn send_mouse(
        &mut self,
        kind: u8,
        x: i32,
        y: i32,
        button: i32,
        delta: f64,
    ) -> Result<(), ApiError> {
        match self {
            #[cfg(target_os = "windows")]
            Self::Windows(w) => w.send_mouse(kind, x, y, button, delta),
            #[cfg(not(target_os = "windows"))]
            Self::Enigo(e) => e.send_mouse(kind, x, y, button, delta),
        }
    }

    fn send_key(&mut self, key_code: i32, down: bool, modifiers: i32) -> Result<(), ApiError> {
        match self {
            #[cfg(target_os = "windows")]
            Self::Windows(w) => w.send_key(key_code, down, modifiers),
            #[cfg(not(target_os = "windows"))]
            Self::Enigo(e) => e.send_key(key_code, down, modifiers),
        }
    }

    fn send_modifier(&mut self, bit: i32, down: bool) -> Result<(), ApiError> {
        match self {
            #[cfg(target_os = "windows")]
            Self::Windows(w) => w.send_modifier(bit, down),
            #[cfg(not(target_os = "windows"))]
            Self::Enigo(e) => e.send_modifier(bit, down),
        }
    }
}

#[cfg(target_os = "windows")]
struct WindowsSendInputInjector;

#[cfg(target_os = "windows")]
impl WindowsSendInputInjector {
    fn new() -> Self {
        Self
    }

    fn send_modifier(&mut self, bit: i32, down: bool) -> Result<(), ApiError> {
        let vk = match bit {
            MOD_SHIFT => 0x10u16,
            MOD_CTRL => 0x11u16,
            MOD_ALT => 0x12u16,
            MOD_META => 0x5Bu16,
            _ => return Ok(()),
        };
        self.send_vk(vk, down, false)
    }

    fn send_key(&mut self, key_code: i32, down: bool, modifiers: i32) -> Result<(), ApiError> {
        if key_code > 0 && key_code < 0x110000 {
            if let Some(ch) = char::from_u32(key_code as u32) {
                if (modifiers & MOD_MASK) == 0 && down && !ch.is_control() {
                    return self.send_unicode(ch);
                }
                if let Some(vk) = vk_from_char(ch) {
                    return self.send_vk(vk, down, false);
                }
            }
        }
        if let Some((vk, extended)) = vk_from_rd_code(key_code) {
            return self.send_vk(vk, down, extended);
        }
        Ok(())
    }

    fn send_mouse(
        &mut self,
        kind: u8,
        x: i32,
        y: i32,
        button: i32,
        delta: f64,
    ) -> Result<(), ApiError> {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
            MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        };

        let v_left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let v_top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let v_w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
        let v_h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);
        let mut vx = (x - v_left).clamp(0, v_w.saturating_sub(1).max(0));
        let mut vy = (y - v_top).clamp(0, v_h.saturating_sub(1).max(0));
        if v_w == 1 {
            vx = 0;
        }
        if v_h == 1 {
            vy = 0;
        }
        let abs_x = ((vx as i64) * 65535 / (v_w.saturating_sub(1).max(1) as i64)) as i32;
        let abs_y = ((vy as i64) * 65535 / (v_h.saturating_sub(1).max(1) as i64)) as i32;

        let mut inputs: Vec<INPUT> = Vec::with_capacity(2);
        inputs.push(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: abs_x,
                    dy: abs_y,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });

        match kind {
            1 => {
                let flags = if button == 2 {
                    MOUSEEVENTF_RIGHTDOWN
                } else {
                    MOUSEEVENTF_LEFTDOWN
                };
                inputs.push(INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: 0,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
            2 => {
                let flags = if button == 2 {
                    MOUSEEVENTF_RIGHTUP
                } else {
                    MOUSEEVENTF_LEFTUP
                };
                inputs.push(INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: 0,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
            3 => {
                let lines = delta.clamp(-32.0, 32.0) as i32;
                if lines != 0 {
                    inputs.push(INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dx: 0,
                                dy: 0,
                                mouseData: (lines * 120) as u32,
                                dwFlags: MOUSEEVENTF_WHEEL,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    });
                }
            }
            4 => {
                let lines = delta.clamp(-32.0, 32.0) as i32;
                if lines != 0 {
                    inputs.push(INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dx: 0,
                                dy: 0,
                                mouseData: (lines * 120) as u32,
                                dwFlags: MOUSEEVENTF_HWHEEL,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    });
                }
            }
            _ => {}
        }

        if inputs.len() == 1 {
            let n = unsafe { SendInput(1, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
            if n != 1 {
                return Err(ApiError::new("RD_INPUT", "SendInput(mouse move) 失败"));
            }
            return Ok(());
        }

        let n = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        if n != inputs.len() as u32 {
            return Err(ApiError::new("RD_INPUT", "SendInput(mouse) 失败"));
        }
        Ok(())
    }

    fn send_unicode(&mut self, ch: char) -> Result<(), ApiError> {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
            SendInput,
        };

        let mut units = [0u16; 2];
        let s = ch.encode_utf16(&mut units);
        let mut inputs: Vec<INPUT> = Vec::with_capacity(s.len() * 2);
        for &u in s.iter() {
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: 0,
                        wScan: u,
                        dwFlags: KEYEVENTF_UNICODE,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: 0,
                        wScan: u,
                        dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
        }
        let n = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        if n != inputs.len() as u32 {
            return Err(ApiError::new("RD_INPUT", "SendInput(unicode) 失败"));
        }
        Ok(())
    }

    fn send_vk(&mut self, vk: u16, down: bool, extended: bool) -> Result<(), ApiError> {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
            SendInput,
        };

        let mut flags = 0u32;
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let n = unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
        if n != 1 {
            return Err(ApiError::new("RD_INPUT", "SendInput(key) 失败"));
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn vk_from_rd_code(code: i32) -> Option<(u16, bool)> {
    Some(match code {
        -100 => (0x25, true),
        -101 => (0x27, true),
        -102 => (0x26, true),
        -103 => (0x28, true),
        -104 => (0x0D, false),
        -105 => (0x08, false),
        -106 => (0x1B, false),
        -107 => (0x2E, true),
        -108 => (0x09, false),
        -109 => (0x24, true),
        -110 => (0x23, true),
        -111 => (0x21, true),
        -112 => (0x22, true),
        _ => return None,
    })
}

#[cfg(target_os = "windows")]
fn vk_from_char(ch: char) -> Option<u16> {
    let up = ch.to_ascii_uppercase();
    if ('A'..='Z').contains(&up) {
        return Some(up as u16);
    }
    if ('0'..='9').contains(&up) {
        return Some(up as u16);
    }
    None
}

#[cfg(not(target_os = "windows"))]
struct EnigoInjector {
    enigo: enigo::Enigo,
}

#[cfg(not(target_os = "windows"))]
impl EnigoInjector {
    fn new() -> Result<Self, ApiError> {
        let enigo = enigo::Enigo::new(&enigo::Settings::default())
            .map_err(|e| ApiError::new("RD_INPUT", format!("Enigo::new: {e}")))?;
        Ok(Self { enigo })
    }

    fn send_modifier(&mut self, bit: i32, down: bool) -> Result<(), ApiError> {
        use enigo::{Direction, Key, Keyboard};
        let k = match bit {
            MOD_SHIFT => Key::Shift,
            MOD_CTRL => Key::Control,
            MOD_ALT => Key::Alt,
            MOD_META => Key::Meta,
            _ => return Ok(()),
        };
        let dir = if down {
            Direction::Press
        } else {
            Direction::Release
        };
        self.enigo
            .key(k, dir)
            .map_err(|e| ApiError::new("RD_INPUT", format!("修饰键: {e}")))?;
        Ok(())
    }

    fn send_mouse(
        &mut self,
        kind: u8,
        x: i32,
        y: i32,
        button: i32,
        delta: f64,
    ) -> Result<(), ApiError> {
        use enigo::{Axis, Button, Coordinate, Direction, Mouse};
        self.enigo
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| ApiError::new("RD_INPUT", format!("鼠标移动: {e}")))?;
        match kind {
            1 => {
                let b = if button == 2 {
                    Button::Right
                } else {
                    Button::Left
                };
                self.enigo
                    .button(b, Direction::Press)
                    .map_err(|e| ApiError::new("RD_INPUT", format!("鼠标按下: {e}")))?;
            }
            2 => {
                let b = if button == 2 {
                    Button::Right
                } else {
                    Button::Left
                };
                self.enigo
                    .button(b, Direction::Release)
                    .map_err(|e| ApiError::new("RD_INPUT", format!("鼠标松开: {e}")))?;
            }
            3 => {
                let lines = delta.clamp(-32.0, 32.0) as i32;
                self.enigo
                    .scroll(lines, Axis::Vertical)
                    .map_err(|e| ApiError::new("RD_INPUT", format!("滚轮: {e}")))?;
            }
            _ => {}
        }
        Ok(())
    }

    fn send_key(&mut self, key_code: i32, down: bool, modifiers: i32) -> Result<(), ApiError> {
        use enigo::{Direction, Key, Keyboard};
        let dir = if down {
            Direction::Press
        } else {
            Direction::Release
        };
        if let Some(k) = rd_key_to_enigo(key_code) {
            self.enigo
                .key(k, dir)
                .map_err(|e| ApiError::new("RD_INPUT", format!("按键: {e}")))?;
            return Ok(());
        }
        if key_code > 0 && key_code < 0x110000 {
            if let Some(ch) = char::from_u32(key_code as u32) {
                if !ch.is_control() || ch == '\n' || ch == '\r' || ch == '\t' {
                    if modifiers != 0 {
                        if let Some(k) = unicode_to_enigo_key(ch) {
                            self.enigo
                                .key(k, dir)
                                .map_err(|e| ApiError::new("RD_INPUT", format!("组合键: {e}")))?;
                        } else {
                            self.enigo
                                .key(Key::Unicode(ch), dir)
                                .map_err(|e| ApiError::new("RD_INPUT", format!("组合键: {e}")))?;
                        }
                    } else if down {
                        let s = ch.to_string();
                        self.enigo
                            .text(&s)
                            .map_err(|e| ApiError::new("RD_INPUT", format!("字符输入: {e}")))?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
fn rd_key_to_enigo(key_code: i32) -> Option<enigo::Key> {
    Some(match key_code {
        -100 => enigo::Key::LeftArrow,
        -101 => enigo::Key::RightArrow,
        -102 => enigo::Key::UpArrow,
        -103 => enigo::Key::DownArrow,
        -104 => enigo::Key::Return,
        -105 => enigo::Key::Backspace,
        -106 => enigo::Key::Escape,
        -107 => enigo::Key::Delete,
        -108 => enigo::Key::Tab,
        -109 => enigo::Key::Home,
        -110 => enigo::Key::End,
        -111 => enigo::Key::PageUp,
        -112 => enigo::Key::PageDown,
        _ => return None,
    })
}

#[cfg(not(target_os = "windows"))]
fn unicode_to_enigo_key(ch: char) -> Option<enigo::Key> {
    if ch.is_ascii() && !ch.is_control() {
        Some(enigo::Key::Unicode(ch))
    } else {
        None
    }
}
