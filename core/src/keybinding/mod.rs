//! 自定义键位映射系统（Phase 1）
//!
//! 设计目标：
//! - 单键映射，零学习成本
//! - 全局 + 页面级两层作用域
//! - 完全兼容现有硬编码行为（默认配置 = 当前键位）
//! - 可扩展至多键序列（Phase 2）

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};

// =============================================================================
// Action 枚举：所有可绑定的动作
// =============================================================================

/// 可绑定的用户动作。
///
/// 命名约定：`<domain>_<verb>`，如 `global_quit`、`search_select_down`。
/// 这样即使不同页面有同名动作（如上下导航），也可以通过域名区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    // --- 全局动作 ---
    /// 退出应用
    GlobalQuit,
    /// 播放/暂停
    GlobalPlayPause,
    /// 下一首
    GlobalNextTrack,
    /// 上一首
    GlobalPrevTrack,
    /// 切换播放模式（列表循环 / 单曲循环 / 随机）
    GlobalCycleMode,
    /// 快进 5 秒
    GlobalSeekForward,
    /// 后退 5 秒
    GlobalSeekBackward,
    /// 音量增加
    GlobalVolumeUp,
    /// 音量减少
    GlobalVolumeDown,
    /// 下一个标签页
    GlobalNextTab,
    /// 上一个标签页
    GlobalPrevTab,
    /// 返回主页面（Esc）
    GlobalGoToMain,
    /// 收藏/取消收藏当前歌曲（Ctrl+L）
    GlobalToggleFavorite,

    // --- 通用列表动作（多个页面共用） ---
    /// 选择上一项
    ListSelectUp,
    /// 选择下一项
    ListSelectDown,
    /// 跳到第一项
    ListSelectFirst,
    /// 跳到最后一项
    ListSelectLast,
    /// 向上翻页
    ListPageUp,
    /// 向下翻页
    ListPageDown,
    /// 激活选中项（播放 / 进入）
    ListActivate,
    /// 返回/退出（Esc）
    ListGoBack,
    /// 添加到队列尾部
    ListAddToQueue,
    /// 添加到队列下一首
    ListAddToQueueNext,

    // --- 搜索页面专用 ---
    /// 进入搜索输入模式
    SearchInputMode,
    /// 开始搜索 / 播放结果（Enter 的复合语义）
    SearchStart,
    /// 切换聚合/单音源搜索
    SearchToggleAggregate,
    /// 切换上一个音源
    SearchCycleSourcePrev,
    /// 切换下一个音源
    SearchCycleSourceNext,

    // --- 本地音乐页面专用 ---
    /// 重新扫描本地音乐目录
    LocalRescan,
    /// 删除选中的本地文件（弹出确认）
    LocalDelete,
    /// 进入过滤模式
    LocalFilter,

    // --- 收藏页面专用 ---
    /// 进入过滤模式
    FavoritesFilter,
    /// 取消收藏
    FavoritesRemove,

    // --- 设置页面专用 ---
    /// 切换设置项的值
    SettingsToggle,
}

// =============================================================================
// KeybindingConfig：序列化配置结构
// =============================================================================

/// 键位配置根结构。
///
/// TOML 示例：
/// ```toml
/// [keybindings.global]
/// quit = "q"
/// play_pause = "Space"
/// next_track = "n"
/// prev_track = "b"
///
/// [keybindings.pages.search]
/// select_up = "k"
/// select_down = "j"
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct KeybindingConfig {
    /// 全局键位映射：动作 -> 键位字符串
    #[serde(default = "default_global_bindings")]
    pub global: HashMap<Action, String>,
    /// 页面级键位映射：页面名 -> (动作 -> 键位字符串)
    #[serde(default = "default_page_bindings")]
    pub pages: HashMap<String, HashMap<Action, String>>,
}

impl Default for KeybindingConfig {
    fn default() -> Self {
        Self {
            global: default_global_bindings(),
            pages: default_page_bindings(),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct PartialKeybindingConfig {
    global: HashMap<Action, String>,
    pages: HashMap<String, HashMap<Action, String>>,
}

impl<'de> Deserialize<'de> for KeybindingConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let partial = PartialKeybindingConfig::deserialize(deserializer)?;
        let mut config = Self::default();
        config.global.extend(partial.global);
        for (page, bindings) in partial.pages {
            config.pages.entry(page).or_default().extend(bindings);
        }
        Ok(config)
    }
}

/// 默认全局键位（与现有硬编码完全一致）
fn default_global_bindings() -> HashMap<Action, String> {
    let mut m = HashMap::new();
    m.insert(Action::GlobalQuit, "q".to_string());
    m.insert(Action::GlobalPlayPause, "Space".to_string());
    m.insert(Action::GlobalNextTrack, "n".to_string());
    m.insert(Action::GlobalPrevTrack, "b".to_string());
    m.insert(Action::GlobalCycleMode, "m".to_string());
    m.insert(Action::GlobalSeekForward, "]".to_string());
    m.insert(Action::GlobalSeekBackward, "[".to_string());
    m.insert(Action::GlobalVolumeUp, ".".to_string());
    m.insert(Action::GlobalVolumeDown, ",".to_string());
    m.insert(Action::GlobalNextTab, "Tab".to_string());
    m.insert(Action::GlobalPrevTab, "Shift+Tab".to_string());
    m.insert(Action::GlobalGoToMain, "Esc".to_string());
    m.insert(Action::GlobalToggleFavorite, "Ctrl+l".to_string());
    m
}

/// 默认页面级键位（与现有硬编码完全一致）
fn default_page_bindings() -> HashMap<String, HashMap<Action, String>> {
    let mut pages = HashMap::new();

    // --- 搜索页面 ---
    let mut search = HashMap::new();
    search.insert(Action::SearchInputMode, "i".to_string());
    search.insert(Action::SearchStart, "Enter".to_string());
    search.insert(Action::SearchToggleAggregate, "v".to_string());
    search.insert(Action::ListSelectUp, "k".to_string());
    search.insert(Action::ListSelectDown, "j".to_string());
    search.insert(Action::ListSelectFirst, "g".to_string());
    search.insert(Action::ListSelectLast, "G".to_string());
    search.insert(Action::ListPageUp, "Ctrl+u".to_string());
    search.insert(Action::ListPageDown, "Ctrl+d".to_string());
    search.insert(Action::ListActivate, "l".to_string());
    search.insert(Action::ListAddToQueue, "a".to_string());
    search.insert(Action::ListAddToQueueNext, "A".to_string());
    search.insert(Action::SearchCycleSourcePrev, "Left".to_string());
    search.insert(Action::SearchCycleSourceNext, "Right".to_string());
    search.insert(Action::ListGoBack, "Esc".to_string());
    pages.insert("search".to_string(), search);

    // --- 主页（队列） ---
    let mut main = HashMap::new();
    main.insert(Action::ListSelectUp, "k".to_string());
    main.insert(Action::ListSelectDown, "j".to_string());
    main.insert(Action::ListSelectFirst, "g".to_string());
    main.insert(Action::ListSelectLast, "G".to_string());
    main.insert(Action::ListPageUp, "Ctrl+u".to_string());
    main.insert(Action::ListPageDown, "Ctrl+d".to_string());
    main.insert(Action::ListActivate, "Enter".to_string());
    pages.insert("main".to_string(), main);

    // --- 排行榜 ---
    let mut leaderboard = HashMap::new();
    leaderboard.insert(Action::ListSelectUp, "k".to_string());
    leaderboard.insert(Action::ListSelectDown, "j".to_string());
    leaderboard.insert(Action::ListSelectFirst, "g".to_string());
    leaderboard.insert(Action::ListSelectLast, "G".to_string());
    leaderboard.insert(Action::ListPageUp, "Ctrl+u".to_string());
    leaderboard.insert(Action::ListPageDown, "Ctrl+d".to_string());
    leaderboard.insert(Action::ListActivate, "Enter".to_string());
    leaderboard.insert(Action::ListAddToQueue, "a".to_string());
    leaderboard.insert(Action::ListAddToQueueNext, "A".to_string());
    leaderboard.insert(Action::SearchCycleSourcePrev, "Left".to_string());
    leaderboard.insert(Action::SearchCycleSourceNext, "Right".to_string());
    leaderboard.insert(Action::ListGoBack, "Esc".to_string());
    pages.insert("leaderboard".to_string(), leaderboard);

    // --- 歌单 ---
    let mut playlists = HashMap::new();
    playlists.insert(Action::ListSelectUp, "k".to_string());
    playlists.insert(Action::ListSelectDown, "j".to_string());
    playlists.insert(Action::ListSelectFirst, "g".to_string());
    playlists.insert(Action::ListSelectLast, "G".to_string());
    playlists.insert(Action::ListPageUp, "Ctrl+u".to_string());
    playlists.insert(Action::ListPageDown, "Ctrl+d".to_string());
    playlists.insert(Action::ListActivate, "Enter".to_string());
    playlists.insert(Action::ListAddToQueue, "a".to_string());
    playlists.insert(Action::ListAddToQueueNext, "A".to_string());
    playlists.insert(Action::SearchCycleSourcePrev, "Left".to_string());
    playlists.insert(Action::SearchCycleSourceNext, "Right".to_string());
    playlists.insert(Action::ListGoBack, "Esc".to_string());
    pages.insert("playlists".to_string(), playlists);

    // --- 收藏 ---
    let mut favorites = HashMap::new();
    favorites.insert(Action::FavoritesFilter, "/".to_string());
    favorites.insert(Action::ListSelectUp, "k".to_string());
    favorites.insert(Action::ListSelectDown, "j".to_string());
    favorites.insert(Action::ListSelectFirst, "g".to_string());
    favorites.insert(Action::ListSelectLast, "G".to_string());
    favorites.insert(Action::ListPageUp, "Ctrl+u".to_string());
    favorites.insert(Action::ListPageDown, "Ctrl+d".to_string());
    favorites.insert(Action::ListActivate, "Enter".to_string());
    favorites.insert(Action::ListAddToQueue, "a".to_string());
    favorites.insert(Action::ListAddToQueueNext, "A".to_string());
    favorites.insert(Action::FavoritesRemove, "d".to_string());
    favorites.insert(Action::ListGoBack, "Esc".to_string());
    pages.insert("favorites".to_string(), favorites);

    // --- 历史 ---
    let mut history = HashMap::new();
    history.insert(Action::ListSelectUp, "k".to_string());
    history.insert(Action::ListSelectDown, "j".to_string());
    history.insert(Action::ListSelectFirst, "g".to_string());
    history.insert(Action::ListSelectLast, "G".to_string());
    history.insert(Action::ListPageUp, "Ctrl+u".to_string());
    history.insert(Action::ListPageDown, "Ctrl+d".to_string());
    history.insert(Action::ListActivate, "Enter".to_string());
    history.insert(Action::ListAddToQueue, "a".to_string());
    history.insert(Action::ListAddToQueueNext, "A".to_string());
    pages.insert("history".to_string(), history);

    // --- 本地音乐 ---
    let mut local = HashMap::new();
    local.insert(Action::ListSelectUp, "k".to_string());
    local.insert(Action::ListSelectDown, "j".to_string());
    local.insert(Action::ListSelectFirst, "g".to_string());
    local.insert(Action::ListSelectLast, "G".to_string());
    local.insert(Action::ListPageUp, "Ctrl+u".to_string());
    local.insert(Action::ListPageDown, "Ctrl+d".to_string());
    local.insert(Action::ListActivate, "Enter".to_string());
    local.insert(Action::ListAddToQueue, "a".to_string());
    local.insert(Action::ListAddToQueueNext, "A".to_string());
    local.insert(Action::LocalRescan, "r".to_string());
    local.insert(Action::LocalDelete, "d".to_string());
    local.insert(Action::LocalFilter, "/".to_string());
    pages.insert("local".to_string(), local);

    // --- 设置 ---
    let mut settings = HashMap::new();
    settings.insert(Action::ListSelectUp, "k".to_string());
    settings.insert(Action::ListSelectDown, "j".to_string());
    settings.insert(Action::ListGoBack, "Esc".to_string());
    pages.insert("settings".to_string(), settings);

    // --- B站登录 ---
    let mut bili_login = HashMap::new();
    bili_login.insert(Action::ListGoBack, "Esc".to_string());
    pages.insert("bili_login".to_string(), bili_login);

    pages
}

// =============================================================================
// KeyBinding：运行时解析后的键位表示
// =============================================================================

/// 解析后的键位，可直接与 `KeyEvent` 匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyBinding {
    pub modifiers: KeyModifiers,
    pub code: KeyCode,
}

impl KeyBinding {
    /// 从 `KeyEvent` 创建
    pub fn from_event(event: KeyEvent) -> Self {
        normalize_char_binding(event.modifiers, event.code)
    }

    /// 匹配一个 `KeyEvent`（忽略 kind 和 state）
    pub fn matches(&self, event: &KeyEvent) -> bool {
        self.code == event.code && self.modifiers == event.modifiers
    }
}

// =============================================================================
// 解析器：字符串 -> KeyBinding
// =============================================================================

/// 解析键位描述字符串，如 `"Ctrl+l"`、`"Space"`、`"Shift+Tab"`。
///
/// 支持的修饰符前缀：`Ctrl+`, `Shift+`, `Alt+`, `Ctrl+Shift+` 等组合。
/// 支持的特殊键名：
/// - `Space`, `Tab`, `BackTab`, `Enter`, `Esc`, `Backspace`
/// - `Up`, `Down`, `Left`, `Right`
/// - `Home`, `End`, `PageUp`, `PageDown`
/// - `Insert`, `Delete`
/// - `F1` ~ `F12`
pub fn parse_keybinding(desc: &str) -> Option<KeyBinding> {
    let desc = desc.trim();
    if desc.is_empty() {
        return None;
    }

    // 解析修饰符
    let mut modifiers = KeyModifiers::NONE;
    let mut remaining = desc;

    loop {
        if let Some(rest) = remaining.strip_prefix("Ctrl+") {
            modifiers |= KeyModifiers::CONTROL;
            remaining = rest;
        } else if let Some(rest) = remaining.strip_prefix("Shift+") {
            modifiers |= KeyModifiers::SHIFT;
            remaining = rest;
        } else if let Some(_rest) = remaining.strip_prefix("Alt+") {
            modifiers |= KeyModifiers::ALT;
            remaining = _rest;
        } else {
            break;
        }
    }

    // 特殊处理：Shift+Tab 在 crossterm 中报告为 BackTab + SHIFT
    if modifiers == KeyModifiers::SHIFT && remaining == "Tab" {
        return Some(KeyBinding {
            modifiers: KeyModifiers::SHIFT,
            code: KeyCode::BackTab,
        });
    }

    // 解析键码
    let code = parse_keycode(remaining)?;

    Some(normalize_char_binding(modifiers, code))
}

fn normalize_char_binding(mut modifiers: KeyModifiers, mut code: KeyCode) -> KeyBinding {
    if let KeyCode::Char(character) = code
        && modifiers.contains(KeyModifiers::SHIFT)
    {
        modifiers.remove(KeyModifiers::SHIFT);
        code = KeyCode::Char(if character.is_ascii_lowercase() {
            character.to_ascii_uppercase()
        } else {
            character
        });
    }
    KeyBinding { modifiers, code }
}

fn parse_keycode(s: &str) -> Option<KeyCode> {
    match s {
        "Space" | " " => Some(KeyCode::Char(' ')),
        "Tab" => Some(KeyCode::Tab),
        "BackTab" => Some(KeyCode::BackTab),
        "Enter" | "Return" => Some(KeyCode::Enter),
        "Esc" | "Escape" => Some(KeyCode::Esc),
        "Backspace" => Some(KeyCode::Backspace),
        "Up" => Some(KeyCode::Up),
        "Down" => Some(KeyCode::Down),
        "Left" => Some(KeyCode::Left),
        "Right" => Some(KeyCode::Right),
        "Home" => Some(KeyCode::Home),
        "End" => Some(KeyCode::End),
        "PageUp" => Some(KeyCode::PageUp),
        "PageDown" => Some(KeyCode::PageDown),
        "Insert" => Some(KeyCode::Insert),
        "Delete" => Some(KeyCode::Delete),
        "CapsLock" => Some(KeyCode::CapsLock),
        "Null" => Some(KeyCode::Null),
        _ => {
            // 尝试解析 F1-F12
            if s.len() >= 2
                && s.starts_with('F')
                && let Ok(n) = s[1..].parse::<u8>()
                && (1..=12).contains(&n)
            {
                return Some(KeyCode::F(n));
            }
            // 尝试解析单个字符
            if s.len() == 1 {
                let c = s.chars().next().unwrap();
                return Some(KeyCode::Char(c));
            }
            None
        }
    }
}

// =============================================================================
// KeybindingResolver：运行时快速查表
// =============================================================================

/// 运行时键位解析器。
///
/// 把配置中的字符串键位预解析为 `HashMap<KeyBinding, Action>`，
/// 实现 O(1) 的事件到动作查找。
pub struct KeybindingResolver {
    global: HashMap<KeyBinding, Action>,
    pages: HashMap<String, HashMap<KeyBinding, Action>>,
}

impl KeybindingResolver {
    /// 从配置创建解析器
    pub fn from_config(config: &KeybindingConfig) -> Self {
        let mut global = HashMap::new();
        for (action, key_str) in &config.global {
            if let Some(binding) = parse_keybinding(key_str) {
                global.insert(binding, *action);
            }
        }

        let mut pages = HashMap::new();
        for (page_name, page_bindings) in &config.pages {
            let mut page_map = HashMap::new();
            for (action, key_str) in page_bindings {
                if let Some(binding) = parse_keybinding(key_str) {
                    page_map.insert(binding, *action);
                }
            }
            pages.insert(page_name.clone(), page_map);
        }

        Self { global, pages }
    }

    /// 解析全局键位事件
    pub fn resolve_global(&self, event: &KeyEvent) -> Option<Action> {
        let binding = KeyBinding::from_event(*event);
        self.global.get(&binding).copied()
    }

    /// 解析页面级键位事件
    pub fn resolve_page(&self, page: &str, event: &KeyEvent) -> Option<Action> {
        let binding = KeyBinding::from_event(*event);
        self.pages
            .get(page)
            .and_then(|map| map.get(&binding).copied())
    }

    /// 同时查询全局和页面级（页面级优先）
    pub fn resolve(&self, page: &str, event: &KeyEvent) -> Option<Action> {
        self.resolve_page(page, event)
            .or_else(|| self.resolve_global(event))
    }
}

// =============================================================================
// 工具函数
// =============================================================================

/// 生成 Colemak 布局预设配置。
///
/// 可作为 `config.toml` 中 `[keybindings]` 节的参考示例。
pub fn colemak_preset() -> KeybindingConfig {
    let mut config = KeybindingConfig::default();
    config
        .global
        .insert(Action::GlobalNextTrack, "k".to_string());
    config
        .global
        .insert(Action::GlobalPrevTrack, "h".to_string());

    for page in [
        "search",
        "main",
        "leaderboard",
        "playlists",
        "favorites",
        "history",
        "local",
        "settings",
    ] {
        if let Some(bindings) = config.pages.get_mut(page) {
            bindings.insert(Action::ListSelectUp, "e".to_string());
            bindings.insert(Action::ListSelectDown, "n".to_string());
        }
    }

    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_char() {
        let b = parse_keybinding("q").unwrap();
        assert_eq!(b.code, KeyCode::Char('q'));
        assert_eq!(b.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn parse_ctrl_combo() {
        let b = parse_keybinding("Ctrl+l").unwrap();
        assert_eq!(b.code, KeyCode::Char('l'));
        assert_eq!(b.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_shift_tab() {
        let b = parse_keybinding("Shift+Tab").unwrap();
        assert_eq!(b.code, KeyCode::BackTab);
        assert_eq!(b.modifiers, KeyModifiers::SHIFT);
    }

    #[test]
    fn parse_space() {
        let b = parse_keybinding("Space").unwrap();
        assert_eq!(b.code, KeyCode::Char(' '));
        assert_eq!(b.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn parse_f_key() {
        let b = parse_keybinding("F5").unwrap();
        assert_eq!(b.code, KeyCode::F(5));
    }

    #[test]
    fn resolver_lookup() {
        let mut config = KeybindingConfig::default();
        config.global.insert(Action::GlobalQuit, "q".to_string());

        let resolver = KeybindingResolver::from_config(&config);
        let event = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(resolver.resolve_global(&event), Some(Action::GlobalQuit));
    }

    #[test]
    fn resolver_page_priority() {
        let mut config = KeybindingConfig::default();
        config
            .global
            .insert(Action::ListSelectDown, "j".to_string());

        let mut search = HashMap::new();
        search.insert(Action::ListSelectDown, "n".to_string());
        config.pages.insert("search".to_string(), search);

        let resolver = KeybindingResolver::from_config(&config);
        let event = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(
            resolver.resolve("search", &event),
            Some(Action::ListSelectDown)
        );
    }

    #[test]
    fn default_config_contains_all_default_scopes() {
        let config = KeybindingConfig::default();

        assert_eq!(
            config.global.get(&Action::GlobalQuit),
            Some(&"q".to_string())
        );
        assert_eq!(
            config
                .pages
                .get("local")
                .and_then(|page| page.get(&Action::LocalRescan)),
            Some(&"r".to_string())
        );
    }

    #[test]
    fn partial_config_keeps_unspecified_defaults() {
        let config: KeybindingConfig = serde_json::from_value(serde_json::json!({
            "global": {
                "global_quit": "Ctrl+q"
            },
            "pages": {
                "local": {
                    "list_select_up": "e"
                }
            }
        }))
        .unwrap();

        assert_eq!(
            config.global.get(&Action::GlobalQuit),
            Some(&"Ctrl+q".to_string())
        );
        assert_eq!(
            config.global.get(&Action::GlobalPlayPause),
            Some(&"Space".to_string())
        );
        assert_eq!(
            config
                .pages
                .get("local")
                .and_then(|page| page.get(&Action::ListSelectUp)),
            Some(&"e".to_string())
        );
        assert_eq!(
            config
                .pages
                .get("local")
                .and_then(|page| page.get(&Action::ListSelectDown)),
            Some(&"j".to_string())
        );
        assert!(config.pages.contains_key("search"));
    }

    #[test]
    fn uppercase_binding_matches_kitty_shift_event() {
        let mut config = KeybindingConfig::default();
        config
            .pages
            .get_mut("local")
            .unwrap()
            .insert(Action::ListSelectLast, "G".to_string());
        let resolver = KeybindingResolver::from_config(&config);
        let event = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);

        assert_eq!(
            resolver.resolve_page("local", &event),
            Some(Action::ListSelectLast)
        );
    }

    #[test]
    fn colemak_preset_keeps_defaults_and_updates_every_list_page() {
        let config = colemak_preset();

        assert_eq!(
            config.global.get(&Action::GlobalPlayPause),
            Some(&"Space".to_string())
        );
        for page in [
            "search",
            "main",
            "leaderboard",
            "playlists",
            "favorites",
            "history",
            "local",
            "settings",
        ] {
            let bindings = config.pages.get(page).unwrap();
            assert_eq!(bindings.get(&Action::ListSelectUp), Some(&"e".to_string()));
            assert_eq!(
                bindings.get(&Action::ListSelectDown),
                Some(&"n".to_string())
            );
        }
        assert_eq!(
            config
                .pages
                .get("local")
                .and_then(|page| page.get(&Action::LocalRescan)),
            Some(&"r".to_string())
        );
    }
}
