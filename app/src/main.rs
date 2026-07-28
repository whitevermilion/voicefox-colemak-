//! voicefox: Rust TUI 版 lx-music-desktop
//!
//! 入口：CLI 解析 → 初始化 → 启动 TUI

mod cli;
mod config;
mod context;
mod cover;
#[cfg(target_os = "linux")]
mod mpris;
mod notification;
mod pages;
mod playlist;
mod storage;
mod theme;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use lx_core::events::{AppAction, InsertPosition, Notification};
use lx_core::keybinding::{Action, KeybindingResolver};
use lx_core::model::leaderboard::LeaderboardInfo;
use lx_core::model::playlist::Playlist;
use lx_core::model::song::SongInfo;
use lx_core::model::source::{Quality, SourceId};
use lx_core::traits::player::PlayerEvent;
use lx_core::traits::source::SongUrl;
use ratatui::DefaultTerminal;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::Style;
use tokio::sync::mpsc;

use context::AppContext;
use pages::components;
use pages::components::context_menu::{MenuOutcome, SongContextMenu, SongMenuAction, SongMenuKind};
use pages::sidebar::NavTab;

enum LeaderboardResponse {
    Boards {
        request_id: u64,
        source: SourceId,
        result: Result<Vec<LeaderboardInfo>, String>,
    },
    Songs {
        request_id: u64,
        source: SourceId,
        board_id: String,
        result: Result<Vec<SongInfo>, String>,
    },
}

enum PlaylistResponse {
    List {
        request_id: u64,
        source: SourceId,
        result: Result<Vec<Playlist>, String>,
    },
    Songs {
        request_id: u64,
        source: SourceId,
        playlist_id: String,
        result: Result<Vec<SongInfo>, String>,
    },
}

#[derive(Debug, Default, Clone, Copy)]
struct UiAreas {
    tabs: Rect,
    content: Rect,
    progress: Rect,
    notification: Rect,
}

#[derive(Debug, Default)]
struct ClickTracker {
    last_left_click: Option<(Instant, u16, u16)>,
}

#[derive(Debug, Clone)]
struct LocalDeleteConfirmation {
    name: String,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteConfirmationAction {
    Confirm,
    Cancel,
    Ignore,
}

impl ClickTracker {
    fn is_double_click(&mut self, event: MouseEvent) -> bool {
        if !matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
            return false;
        }
        let doubled = self.last_left_click.is_some_and(|(time, x, y)| {
            x == event.column && y == event.row && time.elapsed() < Duration::from_millis(500)
        });
        self.last_left_click = if doubled {
            None
        } else {
            Some((Instant::now(), event.column, event.row))
        };
        doubled
    }
}

fn delete_confirmation_action(key: &crossterm::event::KeyEvent) -> DeleteConfirmationAction {
    match (key.modifiers, key.code) {
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('y' | 'Y')) => {
            DeleteConfirmationAction::Confirm
        }
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('n' | 'N'))
        | (KeyModifiers::NONE, KeyCode::Esc) => DeleteConfirmationAction::Cancel,
        _ => DeleteConfirmationAction::Ignore,
    }
}

fn nav_page_scope(tab: NavTab) -> &'static str {
    match tab {
        NavTab::Main => "main",
        NavTab::Search => "search",
        NavTab::Leaderboard => "leaderboard",
        NavTab::Playlists => "playlists",
        NavTab::Favorites => "favorites",
        NavTab::History => "history",
        NavTab::LocalMusic => "local",
        NavTab::Settings => "settings",
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_song_menu_action(
    action: SongMenuAction,
    menu: &SongContextMenu,
    main_page: &mut pages::main_page::MainPage,
    ctx: &AppContext,
    rt: &tokio::runtime::Runtime,
    action_tx: &mpsc::UnboundedSender<AppAction>,
    search_page: &Arc<std::sync::Mutex<pages::search::SearchPage>>,
    settings_page: &Arc<std::sync::Mutex<pages::settings::SettingsPage>>,
    search_seq: &Arc<AtomicU64>,
    confirm_delete: &mut Option<LocalDeleteConfirmation>,
) {
    let app_action = match action {
        SongMenuAction::Play => AppAction::PlaySong {
            songs: menu.songs().to_vec(),
            index: menu.index(),
        },
        SongMenuAction::PlayNext => AppAction::AddToQueue {
            song: Box::new(menu.song().clone()),
            position: InsertPosition::Next,
        },
        SongMenuAction::AddToQueue => AppAction::AddToQueue {
            song: Box::new(menu.song().clone()),
            position: InsertPosition::End,
        },
        SongMenuAction::ToggleFavorite => {
            let song = menu.song();
            let message = if ctx.storage.is_favorite(song) {
                ctx.storage.remove_favorite(song);
                "已取消收藏"
            } else {
                ctx.storage.add_favorite(song);
                "已添加收藏"
            };
            ctx.notify(Notification::success(message));
            AppAction::None
        }
        SongMenuAction::RemoveFromQueue => {
            let action = main_page.remove_at(menu.index(), ctx);
            ctx.notify(Notification::success(format!(
                "已从队列移除: {}",
                menu.song().name
            )));
            action
        }
        SongMenuAction::DeleteLocal => {
            if let Some(path) = &menu.song().file_path {
                *confirm_delete = Some(LocalDeleteConfirmation {
                    name: menu.song().name.clone(),
                    path: path.clone(),
                });
            } else {
                ctx.notify(Notification::error("无法删除：没有本地文件路径"));
            }
            AppAction::None
        }
    };
    execute_action(
        app_action,
        ctx,
        rt,
        action_tx,
        search_page,
        settings_page,
        search_seq,
    );
}

fn main() -> anyhow::Result<()> {
    // 解析 CLI
    let cli = cli::Cli::parse();

    // 加载配置
    let (cfg, config_path) = config::loader::load(&cli.config)?;
    init_logging(&cli.log_level, &config_path);
    tracing::info!(
        "voicefox starting on {} ({})",
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    // 构建 tokio runtime（多线程）
    let rt = tokio::runtime::Runtime::new()?;

    // 初始化 AppContext
    let ctx = rt.block_on(AppContext::new(cfg, config_path))?;

    // 验证 mpv 是否可用
    let mut mpv_check = std::process::Command::new("mpv");
    configure_background_command(&mut mpv_check);
    if mpv_check
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("⚠ 警告: 未找到 mpv，音频播放功能不可用");
        eprintln!("请安装 mpv: sudo apt install mpv (或 brew install mpv)");
    }

    // 启动 TUI
    let mut terminal = ratatui::init();
    let mouse_enabled = ctx.config.read().unwrap().ui.enable_mouse;
    if mouse_enabled {
        let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    }

    // 安装 crossterm panic hook，确保 panic 时 restore 终端
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("fatal panic: {info}");
        if mouse_enabled {
            let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        }
        ratatui::restore();
        original_hook(info);
    }));

    let result = run_app(&mut terminal, ctx, &rt);
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    rt.shutdown_timeout(Duration::from_secs(1));

    result
}

fn init_logging(level: &str, config_path: &Path) {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;

    let log_path = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("voicefox.log");
    let writer = match OpenOptions::new().create(true).append(true).open(log_path) {
        Ok(file) => BoxMakeWriter::new(file),
        Err(_) => BoxMakeWriter::new(std::io::sink),
    };
    let default_filter =
        format!("voicefox={level},lx_source={level},lx_player={level},lx_lyric={level}");
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new(default_filter))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(writer)
        .try_init();
}

fn configure_background_command(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn open_external_url(url: &str) {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };

    configure_background_command(&mut command);
    if let Err(error) = command.spawn() {
        tracing::warn!("failed to open notification URL: {error}");
    }
}

#[allow(unused_assignments)]
fn run_app(
    terminal: &mut DefaultTerminal,
    ctx: AppContext,
    rt: &tokio::runtime::Runtime,
) -> anyhow::Result<()> {
    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<AppAction>();
    let (leaderboard_tx, mut leaderboard_rx) = mpsc::unbounded_channel::<LeaderboardResponse>();
    let (playlist_tx, mut playlist_rx) = mpsc::unbounded_channel::<PlaylistResponse>();
    let mut player_event_rx = ctx.player.take_event_receiver();
    #[cfg(target_os = "linux")]
    let (mpris_handle, mut mpris_command_rx) = if ctx.config.read().unwrap().integration.mpris {
        match rt.block_on(mpris::start()) {
            Ok((handle, receiver)) => (Some(handle), Some(receiver)),
            Err(error) => {
                tracing::warn!("MPRIS unavailable: {error}");
                ctx.notify(Notification::warning(format!("MPRIS 启动失败: {error}")).tui_only());
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    // 搜索请求序列号（用于取消过时请求）
    let search_seq: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let mut leaderboard_request_id: u64 = 0;
    let mut playlist_request_id: u64 = 0;

    // 键位解析器（从配置加载自定义键位）
    let keybindings = ctx.config.read().unwrap().keybindings.clone();
    let kb_resolver = KeybindingResolver::from_config(&keybindings);

    // 导航状态
    let mut active_tab = NavTab::Main;
    let mut observed_active_tab = active_tab;

    // 页面状态
    let (search_source_filter, wrap_navigation, scroll_amount) = {
        let config = ctx.config.read().unwrap();
        (
            if config.ui.aggregate_search {
                None
            } else {
                Some(config.source.default)
            },
            config.ui.wrap_navigation,
            config.ui.scroll_amount,
        )
    };
    let search_page = Arc::new(std::sync::Mutex::new(pages::search::SearchPage::new(
        search_source_filter,
        wrap_navigation,
        scroll_amount,
    )));
    let settings_page = Arc::new(std::sync::Mutex::new(pages::settings::SettingsPage::new()));
    let mut main_page = pages::main_page::MainPage::new();
    let mut leaderboard =
        pages::leaderboard::LeaderboardPage::new(ctx.source_manager.leaderboard_sources());
    let mut playlists = pages::playlists::PlaylistsPage::new(ctx.source_manager.playlist_sources());
    let mut favorites_page = pages::favorites::FavoritesPage::new();
    let mut history_selected: usize = 0;
    let mut history_scroll: usize = 0;
    let mut local_selected: usize = 0;
    let mut local_scroll: usize = 0;
    let mut confirm_delete: Option<LocalDeleteConfirmation> = None;
    let mut song_menu: Option<SongContextMenu> = None;
    let mut ui_areas = UiAreas::default();
    let mut click_tracker = ClickTracker::default();
    let mut bili_login_page: Option<Arc<std::sync::Mutex<pages::bili_login::BiliLoginPage>>> = None;
    let mut bili_poll_deadline: Instant = Instant::now();
    let mut bili_generate_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut bili_poll_task: Option<tokio::task::JoinHandle<()>> = None;

    // 事件驱动渲染：借鉴 rmpc，只在有事件或需要渲染时才 draw()
    let mut needs_render = true;
    // 播放器状态由播放线程改写，没有事件通知，只能靠比对上一轮的值发现变化
    let mut last_player_state = *ctx.player_state.borrow();
    let max_fps = ctx.config.read().unwrap().ui.max_fps.clamp(1, 60);
    let render_interval = Duration::from_millis(1_000 / u64::from(max_fps));
    let mut last_periodic_render = Instant::now();
    let mut last_notification_cleanup = Instant::now();
    let mut mouse_capture_enabled = ctx.config.read().unwrap().ui.enable_mouse;
    #[cfg(target_os = "linux")]
    let mut last_mpris_snapshot: Option<mpris::MprisSnapshot> = None;
    #[cfg(target_os = "linux")]
    let mut last_mpris_update = Instant::now() - Duration::from_secs(1);

    // === 后台异步加载 JS 音源（不阻塞启动） ===
    let js_urls = ctx.config.read().unwrap().source.js_sources.clone();
    let default_source = ctx
        .config
        .read()
        .unwrap()
        .source
        .default
        .as_str()
        .to_string();
    let js_source_generation = ctx.source_manager.begin_js_source_request(false);
    spawn_js_source_loader(
        js_urls,
        default_source,
        Arc::clone(&ctx.source_manager),
        js_source_generation,
        action_tx.clone(),
        rt,
    );

    // === 初始扫描本地音乐 ===
    let local_music_paths = ctx.config.read().unwrap().local_music.paths.clone();
    let local_music_max_depth = ctx.config.read().unwrap().local_music.max_depth;
    if !local_music_paths.is_empty() && ctx.config.read().unwrap().local_music.enabled {
        execute_action(
            AppAction::ScanLocalMusic {
                paths: local_music_paths,
                max_depth: local_music_max_depth,
            },
            &ctx,
            rt,
            &action_tx,
            &search_page,
            &settings_page,
            &search_seq,
        );
    }

    if ctx.bili_source.is_logged_in() {
        let bili_source = Arc::clone(&ctx.bili_source);
        let tx = action_tx.clone();
        rt.spawn(async move {
            match tokio::time::timeout(Duration::from_secs(8), bili_source.login_status()).await {
                Ok(Ok(Some(_))) => {}
                Ok(Ok(None)) => {
                    let _ = tx.send(AppAction::ShowNotification(Notification::warning(
                        "哔哩哔哩登录已失效，请重新扫码",
                    )));
                }
                Ok(Err(error)) => tracing::warn!("validate Bilibili session failed: {error}"),
                Err(_) => tracing::warn!("validate Bilibili session timed out"),
            }
            let _ = tx.send(AppAction::None);
        });
    }

    loop {
        #[cfg(target_os = "linux")]
        if let Some(receiver) = mpris_command_rx.as_mut() {
            while let Ok(command) = receiver.try_recv() {
                if execute_mpris_command(
                    command,
                    &ctx,
                    rt,
                    &action_tx,
                    &search_page,
                    &settings_page,
                    &search_seq,
                ) {
                    tracing::info!("quit requested through MPRIS");
                    ctx.player.stop();
                    ctx.cover_service.clear_display();
                    return Ok(());
                }
                needs_render = true;
            }
        }

        let mouse_requested = ctx.config.read().unwrap().ui.enable_mouse;
        if mouse_requested != mouse_capture_enabled {
            if mouse_requested {
                let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
            } else {
                let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
            }
            mouse_capture_enabled = mouse_requested;
        }

        if observed_active_tab != active_tab {
            let previous_tab = observed_active_tab;
            observed_active_tab = active_tab;
            let local_source = ctx.source_manager.local_source();
            let config = ctx.config.read().unwrap();
            if should_scan_local_music_on_entry(
                previous_tab,
                active_tab,
                config.local_music.enabled,
                !config.local_music.paths.is_empty(),
                local_source.song_count() == 0,
                local_source.is_scanning(),
            ) {
                let action = AppAction::ScanLocalMusic {
                    paths: config.local_music.paths.clone(),
                    max_depth: config.local_music.max_depth,
                };
                drop(config);
                execute_action(
                    action,
                    &ctx,
                    rt,
                    &action_tx,
                    &search_page,
                    &settings_page,
                    &search_seq,
                );
                local_selected = 0;
                local_scroll = 0;
                needs_render = true;
            }
        }

        // === 0. 排空异步 action ===
        while let Ok(action) = action_rx.try_recv() {
            // 拦截哔哩哔哩登录相关 action
            match &action {
                AppAction::BiliLogin => {
                    if let Some(task) = bili_generate_task.take() {
                        task.abort();
                    }
                    if let Some(task) = bili_poll_task.take() {
                        task.abort();
                    }
                    let page = Arc::new(std::sync::Mutex::new(
                        pages::bili_login::BiliLoginPage::new(Arc::clone(&ctx.bili_source)),
                    ));
                    let page_clone = Arc::clone(&page);
                    let wake_tx = action_tx.clone();
                    bili_generate_task = Some(rt.spawn(async move {
                        let result = {
                            let source = Arc::clone(&page_clone.lock().unwrap().source);
                            source.generate_qr_code().await
                        };
                        let mut p = page_clone.lock().unwrap();
                        match result {
                            Ok(qr) => {
                                let qr_lines = pages::bili_login::render_qr_terminal(&qr.url, 1);
                                p.set_waiting(qr.key, qr_lines, qr.expires_in);
                            }
                            Err(e) => p.set_error(format!("生成二维码失败: {e}")),
                        }
                        let _ = wake_tx.send(AppAction::None);
                    }));
                    bili_login_page = Some(page);
                    bili_poll_deadline = Instant::now();
                    needs_render = true;
                    continue;
                }
                AppAction::BiliLoginSuccess => {
                    if let Some(task) = bili_generate_task.take() {
                        task.abort();
                    }
                    if let Some(task) = bili_poll_task.take() {
                        task.abort();
                    }
                    bili_login_page = None;
                    let user = ctx.bili_source.user();
                    let msg = if let Some(user) = user {
                        format!("哔哩哔哩登录成功: {}", user.name)
                    } else {
                        "哔哩哔哩登录成功".to_string()
                    };
                    ctx.notify(Notification::success(msg));
                    needs_render = true;
                    continue;
                }
                AppAction::BiliLogout => {
                    if let Some(task) = bili_generate_task.take() {
                        task.abort();
                    }
                    if let Some(task) = bili_poll_task.take() {
                        task.abort();
                    }
                    bili_login_page = None;
                    let notification = match ctx.bili_source.logout() {
                        Ok(()) => Notification::success("已退出哔哩哔哩登录"),
                        Err(error) => Notification::error(error),
                    };
                    ctx.notify(notification);
                    needs_render = true;
                    continue;
                }
                _ => {}
            }
            execute_action(
                action,
                &ctx,
                rt,
                &action_tx,
                &search_page,
                &settings_page,
                &search_seq,
            );
            needs_render = true;
        }
        if let Some(rx) = player_event_rx.as_mut() {
            while let Ok(event) = rx.try_recv() {
                match event {
                    PlayerEvent::Ended => {
                        if let Some((songs, index)) = ctx.playlist.next_entry() {
                            execute_action(
                                AppAction::PlaySong { songs, index },
                                &ctx,
                                rt,
                                &action_tx,
                                &search_page,
                                &settings_page,
                                &search_seq,
                            );
                        }
                    }
                    PlayerEvent::Error(error) => {
                        let retry_song = ctx.current_song.read().unwrap().clone();
                        let auto_toggle = ctx.config.read().unwrap().source.auto_toggle;
                        if auto_toggle
                            && retry_song
                                .as_ref()
                                .is_some_and(|song| song.source != SourceId::Local)
                        {
                            tracing::warn!("current source playback failed: {}", error);
                            if let Some(song) = retry_song {
                                let _ = action_tx.send(AppAction::ShowNotification(
                                    Notification::warning("当前音源播放失败，正在尝试其他音源"),
                                ));
                                let _ = action_tx.send(AppAction::RetrySong {
                                    song: Box::new(song),
                                });
                            }
                        } else {
                            ctx.notify(Notification::error(format!("播放器错误: {}", error)));
                        }
                    }
                    PlayerEvent::Buffering(_) => {}
                }
                needs_render = true;
            }
        }
        // 排行榜异步结果
        while let Ok(response) = leaderboard_rx.try_recv() {
            match response {
                LeaderboardResponse::Boards {
                    request_id,
                    source,
                    result,
                } if request_id == leaderboard_request_id
                    && leaderboard.current_source() == Some(source) =>
                {
                    match result {
                        Ok(boards) => leaderboard.update_boards(source, boards),
                        Err(error) => {
                            let request =
                                pages::leaderboard::LeaderboardLoadRequest::Boards { source };
                            leaderboard.update_error(&request, error.clone());
                            ctx.notify(Notification::error(format!("加载榜单目录失败: {error}")));
                        }
                    }
                    needs_render = true;
                }
                LeaderboardResponse::Songs {
                    request_id,
                    source,
                    board_id,
                    result,
                } if request_id == leaderboard_request_id
                    && leaderboard.current_source() == Some(source)
                    && leaderboard.current_board().map(|board| board.id.as_str())
                        == Some(board_id.as_str()) =>
                {
                    match result {
                        Ok(songs) => leaderboard.update_songs(source, &board_id, songs),
                        Err(error) => {
                            let request = pages::leaderboard::LeaderboardLoadRequest::Songs {
                                source,
                                board_id,
                            };
                            leaderboard.update_error(&request, error.clone());
                            ctx.notify(Notification::error(format!("加载榜单歌曲失败: {error}")));
                        }
                    }
                    needs_render = true;
                }
                _ => {}
            }
        }
        while let Ok(response) = playlist_rx.try_recv() {
            match response {
                PlaylistResponse::List {
                    request_id,
                    source,
                    result,
                } if request_id == playlist_request_id
                    && playlists.current_source() == Some(source) =>
                {
                    match result {
                        Ok(items) => playlists.update_playlists(source, items),
                        Err(error) => {
                            let request = pages::playlists::PlaylistLoadRequest::List { source };
                            playlists.update_error(&request, error.clone());
                            ctx.notify(Notification::error(format!("加载热门歌单失败: {error}")));
                        }
                    }
                    needs_render = true;
                }
                PlaylistResponse::Songs {
                    request_id,
                    source,
                    playlist_id,
                    result,
                } if request_id == playlist_request_id
                    && playlists
                        .current_playlist()
                        .map(|playlist| (playlist.source, playlist.id.as_str()))
                        == Some((source, playlist_id.as_str())) =>
                {
                    match result {
                        Ok(songs) => playlists.update_songs(source, &playlist_id, songs),
                        Err(error) => {
                            let request = pages::playlists::PlaylistLoadRequest::Songs {
                                source,
                                playlist_id,
                            };
                            playlists.update_error(&request, error.clone());
                            ctx.notify(Notification::error(format!("加载歌单歌曲失败: {error}")));
                        }
                    }
                    needs_render = true;
                }
                _ => {}
            }
        }
        if active_tab == NavTab::Leaderboard {
            maybe_spawn_leaderboard_load(
                &mut leaderboard,
                &mut leaderboard_request_id,
                Arc::clone(&ctx.source_manager),
                leaderboard_tx.clone(),
                rt,
            );
        }
        if active_tab == NavTab::Playlists {
            playlists.sync_favorites(&ctx);
            maybe_spawn_playlist_load(
                &mut playlists,
                &mut playlist_request_id,
                Arc::clone(&ctx.source_manager),
                playlist_tx.clone(),
                rt,
            );
        }

        // === 1. 周期维护 ===
        // 这些工作必须独立于终端事件执行，否则持续按键或拖动鼠标会让
        // 搜索防抖、歌词同步和进度渲染长期得不到运行机会。
        if active_tab == NavTab::Search {
            let action = {
                let mut sp = search_page.lock().unwrap();
                sp.tick()
            };
            if let Some(action) = action {
                execute_action(
                    action,
                    &ctx,
                    rt,
                    &action_tx,
                    &search_page,
                    &settings_page,
                    &search_seq,
                );
                needs_render = true;
            }
        }

        if bili_generate_task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            bili_generate_task.take();
        }
        if bili_poll_task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            bili_poll_task.take();
        }

        if let Some(ref page) = bili_login_page
            && bili_poll_task.is_none()
            && bili_poll_deadline.elapsed() >= Duration::from_secs(2)
        {
            let params = {
                let mut page = page.lock().unwrap();
                if page.should_poll() {
                    page.begin_poll()
                } else {
                    None
                }
            };
            if let Some((source, key)) = params {
                let page = Arc::clone(page);
                let wake_tx = action_tx.clone();
                bili_poll_task = Some(rt.spawn(async move {
                    let result = source.poll_qr_code(&key).await;
                    page.lock().unwrap().apply_check_result(result);
                    let _ = wake_tx.send(AppAction::None);
                }));
                bili_poll_deadline = Instant::now();
                needs_render = true;
            }
        }

        if last_notification_cleanup.elapsed() >= Duration::from_millis(250) {
            let notifications_enabled = ctx.config.read().unwrap().notification.in_app;
            let lifetime = ctx.notification_timeout();
            let mut notifs = ctx.notifications.write().unwrap();
            let previous_len = notifs.len();
            if notifications_enabled {
                notifs.retain(|notification| !notification.is_expired(lifetime));
            } else {
                notifs.clear();
            }
            if notifs.len() != previous_len {
                needs_render = true;
            }
            last_notification_cleanup = Instant::now();
        }

        // borrow 很便宜，所以不受 render_interval 门控
        let state = *ctx.player_state.borrow();
        if state != last_player_state {
            last_player_state = state;
            needs_render = true;
        }
        #[cfg(target_os = "linux")]
        if let Some(handle) = mpris_handle.as_ref()
            && last_mpris_update.elapsed() >= Duration::from_millis(250)
        {
            let snapshot = current_mpris_snapshot(&ctx);
            if last_mpris_snapshot.as_ref() != Some(&snapshot) {
                handle.update(snapshot.clone());
                last_mpris_snapshot = Some(snapshot);
            }
            last_mpris_update = Instant::now();
        }

        if last_periodic_render.elapsed() >= render_interval {
            ctx.lyric_service.update_position(*ctx.position.borrow());
            let input_active = active_tab == NavTab::Search
                && search_page.lock().unwrap().input_mode
                || active_tab == NavTab::Settings && settings_page.lock().unwrap().input_mode
                || active_tab == NavTab::Favorites && favorites_page.input_mode();
            let notification_active = !ctx.notifications.read().unwrap().is_empty();
            needs_render |= matches!(
                state,
                lx_core::model::source::PlayerState::Playing
                    | lx_core::model::source::PlayerState::Loading
            ) || input_active
                || notification_active
                || bili_login_page.is_some();
            last_periodic_render = Instant::now();
        }

        // 在读取下一个事件前先补画上一轮状态。这样即使 key repeat 每轮都
        // 触发 continue，界面也不会被连续输入饿死。
        if needs_render {
            draw_app(
                terminal,
                &ctx,
                active_tab,
                &search_page,
                &settings_page,
                &mut main_page,
                &mut leaderboard,
                &mut playlists,
                &mut favorites_page,
                &mut history_selected,
                &mut history_scroll,
                &mut local_selected,
                &mut local_scroll,
                &mut ui_areas,
                &confirm_delete,
                &song_menu,
                &bili_login_page,
            )?;
            needs_render = false;
        }

        // === 2. 事件驱动：轮询终端事件 ===
        let terminal_event = if event::poll(Duration::from_millis(50)).unwrap_or(false) {
            event::read().ok()
        } else {
            None
        };
        if let Some(Event::Key(key)) = terminal_event.as_ref()
            && key.kind == KeyEventKind::Press
        {
            let key = *key;
            // 1a. 侧边栏全局快捷键 (/, 1-5) — 输入模式下跳过
            let settings_input_mode =
                active_tab == NavTab::Settings && settings_page.lock().unwrap().any_input_active();
            let search_input_mode =
                active_tab == NavTab::Search && search_page.lock().unwrap().input_mode;
            let favorites_input_mode =
                active_tab == NavTab::Favorites && favorites_page.input_mode();
            let text_input_active =
                settings_input_mode || search_input_mode || favorites_input_mode;

            if let Some(ref page) = bili_login_page {
                let action = page.lock().unwrap().handle_input(key, &kb_resolver);
                match action {
                    AppAction::BiliLoginSuccess => {
                        let _ = action_tx.send(AppAction::BiliLoginSuccess);
                    }
                    AppAction::GoBack => {
                        if let Some(task) = bili_generate_task.take() {
                            task.abort();
                        }
                        if let Some(task) = bili_poll_task.take() {
                            task.abort();
                        }
                        bili_login_page = None;
                    }
                    _ => {}
                }
                needs_render = true;
                continue;
            }

            if confirm_delete.is_some() {
                match delete_confirmation_action(&key) {
                    DeleteConfirmationAction::Confirm => {
                        let confirmation = confirm_delete.take().unwrap();
                        match std::fs::remove_file(&confirmation.path) {
                            Ok(()) => {
                                let local_source = ctx.source_manager.local_source();
                                local_source.remove_by_path(&confirmation.path);
                                let remaining = local_source.all_songs().len();
                                local_selected = local_selected.min(remaining.saturating_sub(1));
                                local_scroll = local_scroll.min(remaining.saturating_sub(1));

                                ctx.notify(Notification::success(format!(
                                    "已删除本地文件: {}",
                                    confirmation.name
                                )));
                                let config = ctx.config.read().unwrap();
                                let paths = config.local_music.paths.clone();
                                let max_depth = config.local_music.max_depth;
                                drop(config);
                                execute_action(
                                    AppAction::ScanLocalMusic { paths, max_depth },
                                    &ctx,
                                    rt,
                                    &action_tx,
                                    &search_page,
                                    &settings_page,
                                    &search_seq,
                                );
                            }
                            Err(error) => {
                                ctx.notify(Notification::error(format!(
                                    "删除本地文件失败: {}",
                                    error
                                )));
                            }
                        }
                    }
                    DeleteConfirmationAction::Cancel => {
                        confirm_delete = None;
                    }
                    DeleteConfirmationAction::Ignore => {}
                }
                needs_render = true;
                continue;
            }

            if let Some(menu) = song_menu.as_mut() {
                let outcome = menu.handle_key(&key, &kb_resolver, nav_page_scope(active_tab));
                match outcome {
                    MenuOutcome::None => {}
                    MenuOutcome::Close => {
                        song_menu = None;
                    }
                    MenuOutcome::Action(action) => {
                        let menu = song_menu.take().unwrap();
                        execute_song_menu_action(
                            action,
                            &menu,
                            &mut main_page,
                            &ctx,
                            rt,
                            &action_tx,
                            &search_page,
                            &settings_page,
                            &search_seq,
                            &mut confirm_delete,
                        );
                    }
                }
                needs_render = true;
                continue;
            }

            if !text_input_active
                && let Some(tab) = pages::sidebar::handle_input(&key)
            {
                active_tab = tab;
                needs_render = true;
                continue;
            }

            // 1b. 全局快捷键（查表模式，保留完全相同的默认行为）
            if let Some(action) = kb_resolver.resolve_global(&key) {
                match action {
                    Action::GlobalQuit if !text_input_active => {
                        tracing::info!("quit requested");
                        ctx.player.stop();
                        ctx.cover_service.clear_display();
                        return Ok(());
                    }
                    Action::GlobalPlayPause if !text_input_active => {
                        ctx.player.toggle();
                        needs_render = true;
                        continue;
                    }
                    Action::GlobalNextTrack if !text_input_active => {
                        if let Some((songs, index)) = ctx.playlist.next_manual_entry() {
                            execute_action(
                                AppAction::PlaySong { songs, index },
                                &ctx,
                                rt,
                                &action_tx,
                                &search_page,
                                &settings_page,
                                &search_seq,
                            );
                        }
                        needs_render = true;
                        continue;
                    }
                    Action::GlobalPrevTrack
                        if !text_input_active && active_tab != NavTab::Settings =>
                    {
                        if let Some((songs, index)) = ctx.playlist.prev_manual_entry() {
                            execute_action(
                                AppAction::PlaySong { songs, index },
                                &ctx,
                                rt,
                                &action_tx,
                                &search_page,
                                &settings_page,
                                &search_seq,
                            );
                        }
                        needs_render = true;
                        continue;
                    }
                    Action::GlobalCycleMode if !text_input_active => {
                        let mode = ctx.playlist.cycle_mode();
                        let save_result = {
                            let mut config = ctx.config.write().unwrap();
                            config.player.play_mode = mode.as_config().to_string();
                            crate::config::loader::save(&config, &ctx.config_path)
                        };
                        let notification = match save_result {
                            Ok(()) => Notification::success(format!("播放模式: {}", mode.label())),
                            Err(error) => Notification::error(format!(
                                "播放模式已切换，但保存失败: {}",
                                error
                            )),
                        };
                        ctx.notify(notification);
                        needs_render = true;
                        continue;
                    }
                    Action::GlobalSeekForward
                        if !text_input_active
                            && active_tab != NavTab::Leaderboard
                            && active_tab != NavTab::Playlists =>
                    {
                        let pos = *ctx.position.borrow();
                        ctx.player.seek(pos + Duration::from_secs(5));
                        needs_render = true;
                        continue;
                    }
                    Action::GlobalSeekBackward
                        if !text_input_active
                            && active_tab != NavTab::Leaderboard
                            && active_tab != NavTab::Playlists =>
                    {
                        let pos = *ctx.position.borrow();
                        if pos > Duration::from_secs(5) {
                            ctx.player.seek(pos - Duration::from_secs(5));
                        } else {
                            ctx.player.seek(Duration::ZERO);
                        }
                        needs_render = true;
                        continue;
                    }
                    Action::GlobalVolumeUp if !text_input_active => {
                        ctx.player.volume_up(5);
                        needs_render = true;
                        continue;
                    }
                    Action::GlobalVolumeDown if !text_input_active => {
                        ctx.player.volume_down(5);
                        needs_render = true;
                        continue;
                    }
                    Action::GlobalNextTab if !text_input_active => {
                        active_tab = match active_tab {
                            NavTab::Main => NavTab::Search,
                            NavTab::Search => NavTab::Leaderboard,
                            NavTab::Leaderboard => NavTab::Playlists,
                            NavTab::Playlists => NavTab::Favorites,
                            NavTab::Favorites => NavTab::History,
                            NavTab::History => NavTab::LocalMusic,
                            NavTab::LocalMusic => NavTab::Settings,
                            NavTab::Settings => NavTab::Main,
                        };
                        needs_render = true;
                        continue;
                    }
                    Action::GlobalPrevTab if !text_input_active => {
                        active_tab = match active_tab {
                            NavTab::Main => NavTab::Settings,
                            NavTab::Search => NavTab::Main,
                            NavTab::Leaderboard => NavTab::Search,
                            NavTab::Playlists => NavTab::Leaderboard,
                            NavTab::Favorites => NavTab::Playlists,
                            NavTab::History => NavTab::Favorites,
                            NavTab::LocalMusic => NavTab::History,
                            NavTab::Settings => NavTab::LocalMusic,
                        };
                        needs_render = true;
                        continue;
                    }
                    Action::GlobalGoToMain
                        if !settings_input_mode
                            && active_tab != NavTab::Main
                            && active_tab != NavTab::Search
                            && active_tab != NavTab::Favorites
                            && !(active_tab == NavTab::Playlists
                                && playlists.selected_playlist.is_some())
                            && !(active_tab == NavTab::Leaderboard
                                && leaderboard.selected_board.is_some()) =>
                    {
                        active_tab = NavTab::Main;
                        needs_render = true;
                        continue;
                    }
                    Action::GlobalToggleFavorite
                        if !text_input_active
                            && active_tab != NavTab::Favorites
                            && active_tab != NavTab::Playlists =>
                    {
                        if let Some(song) = ctx.current_song.read().unwrap().as_ref() {
                            if ctx.storage.is_favorite(song) {
                                ctx.storage.remove_favorite(song);
                                let _ = action_tx.send(AppAction::ShowNotification(
                                    Notification::success("已取消收藏"),
                                ));
                            } else {
                                ctx.storage.add_favorite(song);
                                let _ = action_tx.send(AppAction::ShowNotification(
                                    Notification::success("已添加收藏"),
                                ));
                            }
                        }
                        needs_render = true;
                        continue;
                    }
                    _ => {}
                }
            }

            // Fallback：不在自定义配置中的全局键位别名（保持向后兼容）
            match (key.modifiers, key.code) {
                (KeyModifiers::SHIFT, KeyCode::Char('>')) if !text_input_active => {
                    if let Some((songs, index)) = ctx.playlist.next_manual_entry() {
                        execute_action(
                            AppAction::PlaySong { songs, index },
                            &ctx,
                            rt,
                            &action_tx,
                            &search_page,
                            &settings_page,
                            &search_seq,
                        );
                    }
                    needs_render = true;
                    continue;
                }
                (KeyModifiers::SHIFT, KeyCode::Char('<')) if !text_input_active => {
                    if let Some((songs, index)) = ctx.playlist.prev_manual_entry() {
                        execute_action(
                            AppAction::PlaySong { songs, index },
                            &ctx,
                            rt,
                            &action_tx,
                            &search_page,
                            &settings_page,
                            &search_seq,
                        );
                    }
                    needs_render = true;
                    continue;
                }
                (KeyModifiers::NONE, KeyCode::Right)
                    if !text_input_active
                        && active_tab != NavTab::Search
                        && active_tab != NavTab::Leaderboard
                        && active_tab != NavTab::Playlists =>
                {
                    let pos = *ctx.position.borrow();
                    ctx.player.seek(pos + Duration::from_secs(5));
                    needs_render = true;
                    continue;
                }
                (KeyModifiers::NONE, KeyCode::Left)
                    if !text_input_active
                        && active_tab != NavTab::Search
                        && active_tab != NavTab::Leaderboard
                        && active_tab != NavTab::Playlists =>
                {
                    let pos = *ctx.position.borrow();
                    if pos > Duration::from_secs(5) {
                        ctx.player.seek(pos - Duration::from_secs(5));
                    } else {
                        ctx.player.seek(Duration::ZERO);
                    }
                    needs_render = true;
                    continue;
                }
                (KeyModifiers::NONE, KeyCode::Up) if active_tab == NavTab::Main => {
                    ctx.player.volume_up(5);
                    needs_render = true;
                    continue;
                }
                (KeyModifiers::NONE, KeyCode::Down) if active_tab == NavTab::Main => {
                    ctx.player.volume_down(5);
                    needs_render = true;
                    continue;
                }
                _ => {}
            }

            // 1c. 路由到当前页面
            match active_tab {
                NavTab::Search => {
                    let action = {
                        let mut sp = search_page.lock().unwrap();
                        sp.handle_input(key, &kb_resolver)
                    };
                    if matches!(action, AppAction::GoBack) {
                        active_tab = NavTab::Main;
                        needs_render = true;
                        continue;
                    }
                    execute_action(
                        action,
                        &ctx,
                        rt,
                        &action_tx,
                        &search_page,
                        &settings_page,
                        &search_seq,
                    );
                }
                NavTab::Main => {
                    let action = main_page.handle_input(&key, &ctx, &kb_resolver);
                    execute_action(
                        action,
                        &ctx,
                        rt,
                        &action_tx,
                        &search_page,
                        &settings_page,
                        &search_seq,
                    );
                }
                NavTab::Leaderboard => {
                    let action = leaderboard.handle_input(&key, &ctx, &kb_resolver);
                    execute_action(
                        action,
                        &ctx,
                        rt,
                        &action_tx,
                        &search_page,
                        &settings_page,
                        &search_seq,
                    );
                }
                NavTab::Playlists => {
                    let action = playlists.handle_input(&key, &ctx, &kb_resolver);
                    execute_action(
                        action,
                        &ctx,
                        rt,
                        &action_tx,
                        &search_page,
                        &settings_page,
                        &search_seq,
                    );
                }
                NavTab::Favorites => {
                    let action = favorites_page.handle_input(&key, &ctx, &kb_resolver);
                    if matches!(action, AppAction::GoBack) {
                        active_tab = NavTab::Main;
                        needs_render = true;
                        continue;
                    }
                    execute_action(
                        action,
                        &ctx,
                        rt,
                        &action_tx,
                        &search_page,
                        &settings_page,
                        &search_seq,
                    );
                }
                NavTab::History => {
                    let action = pages::history::handle_input(
                        &key,
                        &ctx,
                        &mut history_selected,
                        &kb_resolver,
                    );
                    // 保持在历史页面，不强制切换到主页
                    execute_action(
                        action,
                        &ctx,
                        rt,
                        &action_tx,
                        &search_page,
                        &settings_page,
                        &search_seq,
                    );
                }
                NavTab::Settings => {
                    let action = {
                        let mut sp = settings_page.lock().unwrap();
                        sp.handle_input(key, &ctx, &kb_resolver)
                    };
                    // BiliLogin/BiliLogout 需要发到 channel 让主循环处理（生成 QR 码等）
                    if matches!(
                        action,
                        AppAction::BiliLogin | AppAction::BiliLogout | AppAction::BiliLoginSuccess
                    ) {
                        let _ = action_tx.send(action);
                    } else {
                        if matches!(key.code, KeyCode::Char('g') | KeyCode::Char('w')) {
                            let config = ctx.config.read().unwrap();
                            search_page.lock().unwrap().set_preferences(
                                config.ui.aggregate_search,
                                config.source.default,
                                config.ui.wrap_navigation,
                                config.ui.scroll_amount,
                            );
                        }
                        execute_action(
                            action,
                            &ctx,
                            rt,
                            &action_tx,
                            &search_page,
                            &settings_page,
                            &search_seq,
                        );
                    }
                }
                NavTab::LocalMusic => {
                    let local_src = ctx.source_manager.local_source();
                    let songs = local_src.all_songs();
                    if let Some(action) = kb_resolver.resolve_page("local", &key) {
                        match action {
                            Action::LocalRescan => {
                                let paths = ctx.config.read().unwrap().local_music.paths.clone();
                                let max_depth = ctx.config.read().unwrap().local_music.max_depth;
                                execute_action(
                                    AppAction::ScanLocalMusic { paths, max_depth },
                                    &ctx,
                                    rt,
                                    &action_tx,
                                    &search_page,
                                    &settings_page,
                                    &search_seq,
                                );
                                local_selected = 0;
                                local_scroll = 0;
                            }
                            Action::ListSelectUp => {
                                local_selected = previous_list_index(
                                    local_selected,
                                    songs.len(),
                                    ctx.config.read().unwrap().ui.wrap_navigation,
                                );
                            }
                            Action::ListSelectDown => {
                                local_selected = next_list_index(
                                    local_selected,
                                    songs.len(),
                                    ctx.config.read().unwrap().ui.wrap_navigation,
                                );
                            }
                            Action::ListSelectFirst => {
                                local_selected = 0;
                            }
                            Action::ListSelectLast => {
                                local_selected = songs.len().saturating_sub(1);
                            }
                            Action::ListPageUp => {
                                local_selected = local_selected.saturating_sub(10);
                            }
                            Action::ListPageDown => {
                                local_selected =
                                    (local_selected + 10).min(songs.len().saturating_sub(1));
                            }
                            Action::ListAddToQueue => {
                                if let Some(song) = songs.get(local_selected).cloned() {
                                    execute_action(
                                        AppAction::AddToQueue {
                                            song: Box::new(song),
                                            position: InsertPosition::End,
                                        },
                                        &ctx,
                                        rt,
                                        &action_tx,
                                        &search_page,
                                        &settings_page,
                                        &search_seq,
                                    );
                                }
                            }
                            Action::ListAddToQueueNext => {
                                if let Some(song) = songs.get(local_selected).cloned() {
                                    execute_action(
                                        AppAction::AddToQueue {
                                            song: Box::new(song),
                                            position: InsertPosition::Next,
                                        },
                                        &ctx,
                                        rt,
                                        &action_tx,
                                        &search_page,
                                        &settings_page,
                                        &search_seq,
                                    );
                                }
                            }
                            Action::LocalDelete => {
                                if let Some(song) = songs.get(local_selected) {
                                    if let Some(path) = &song.file_path {
                                        confirm_delete = Some(LocalDeleteConfirmation {
                                            name: song.name.clone(),
                                            path: path.clone(),
                                        });
                                    } else {
                                        ctx.notify(Notification::error(
                                            "无法删除：没有本地文件路径",
                                        ));
                                    }
                                }
                            }
                            Action::ListActivate
                                if !songs.is_empty() && local_selected < songs.len() =>
                            {
                                execute_action(
                                    AppAction::PlaySong {
                                        songs,
                                        index: local_selected,
                                    },
                                    &ctx,
                                    rt,
                                    &action_tx,
                                    &search_page,
                                    &settings_page,
                                    &search_seq,
                                );
                            }
                            _ => {}
                        }
                    } else {
                        match (key.modifiers, key.code) {
                            (KeyModifiers::NONE, KeyCode::Char('r')) => {
                                let paths = ctx.config.read().unwrap().local_music.paths.clone();
                                let max_depth = ctx.config.read().unwrap().local_music.max_depth;
                                execute_action(
                                    AppAction::ScanLocalMusic { paths, max_depth },
                                    &ctx,
                                    rt,
                                    &action_tx,
                                    &search_page,
                                    &settings_page,
                                    &search_seq,
                                );
                                local_selected = 0;
                                local_scroll = 0;
                            }
                            (KeyModifiers::NONE, KeyCode::Up) => {
                                local_selected = previous_list_index(
                                    local_selected,
                                    songs.len(),
                                    ctx.config.read().unwrap().ui.wrap_navigation,
                                );
                            }
                            (KeyModifiers::NONE, KeyCode::Down) => {
                                local_selected = next_list_index(
                                    local_selected,
                                    songs.len(),
                                    ctx.config.read().unwrap().ui.wrap_navigation,
                                );
                            }
                            (KeyModifiers::NONE, KeyCode::Home)
                            | (KeyModifiers::NONE, KeyCode::Char('g')) => {
                                local_selected = 0;
                            }
                            (KeyModifiers::NONE, KeyCode::End)
                            | (KeyModifiers::NONE, KeyCode::Char('G'))
                            | (KeyModifiers::SHIFT, KeyCode::Char('G')) => {
                                local_selected = songs.len().saturating_sub(1);
                            }
                            (KeyModifiers::CONTROL, KeyCode::Char('u'))
                            | (KeyModifiers::NONE, KeyCode::PageUp) => {
                                local_selected = local_selected.saturating_sub(10);
                            }
                            (KeyModifiers::CONTROL, KeyCode::Char('d'))
                            | (KeyModifiers::NONE, KeyCode::PageDown) => {
                                local_selected =
                                    (local_selected + 10).min(songs.len().saturating_sub(1));
                            }
                            _ if pages::is_song_activation_key(&key) => {
                                if !songs.is_empty() && local_selected < songs.len() {
                                    execute_action(
                                        AppAction::PlaySong {
                                            songs,
                                            index: local_selected,
                                        },
                                        &ctx,
                                        rt,
                                        &action_tx,
                                        &search_page,
                                        &settings_page,
                                        &search_seq,
                                    );
                                }
                            }
                            (KeyModifiers::NONE, KeyCode::Char('a')) => {
                                if let Some(song) = songs.get(local_selected).cloned() {
                                    execute_action(
                                        AppAction::AddToQueue {
                                            song: Box::new(song),
                                            position: InsertPosition::End,
                                        },
                                        &ctx,
                                        rt,
                                        &action_tx,
                                        &search_page,
                                        &settings_page,
                                        &search_seq,
                                    );
                                }
                            }
                            (KeyModifiers::NONE, KeyCode::Char('A'))
                            | (KeyModifiers::SHIFT, KeyCode::Char('A')) => {
                                if let Some(song) = songs.get(local_selected).cloned() {
                                    execute_action(
                                        AppAction::AddToQueue {
                                            song: Box::new(song),
                                            position: InsertPosition::Next,
                                        },
                                        &ctx,
                                        rt,
                                        &action_tx,
                                        &search_page,
                                        &settings_page,
                                        &search_seq,
                                    );
                                }
                            }
                            (KeyModifiers::NONE, KeyCode::Char('d'))
                            | (KeyModifiers::NONE, KeyCode::Delete) => {
                                if let Some(song) = songs.get(local_selected) {
                                    if let Some(path) = &song.file_path {
                                        confirm_delete = Some(LocalDeleteConfirmation {
                                            name: song.name.clone(),
                                            path: path.clone(),
                                        });
                                    } else {
                                        ctx.notify(Notification::error(
                                            "无法删除：没有本地文件路径",
                                        ));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            needs_render = true;
        } else if let Some(Event::Mouse(mouse)) = terminal_event.as_ref() {
            if confirm_delete.is_some() {
                needs_render = true;
                continue;
            }
            let mouse = *mouse;

            if let Some(menu) = song_menu.as_mut() {
                let outcome = menu.handle_mouse(mouse, ui_areas.content);
                match outcome {
                    MenuOutcome::None => {}
                    MenuOutcome::Close => {
                        song_menu = None;
                    }
                    MenuOutcome::Action(action) => {
                        let menu = song_menu.take().unwrap();
                        execute_song_menu_action(
                            action,
                            &menu,
                            &mut main_page,
                            &ctx,
                            rt,
                            &action_tx,
                            &search_page,
                            &settings_page,
                            &search_seq,
                            &mut confirm_delete,
                        );
                    }
                }
                needs_render = true;
                continue;
            }

            let activate = click_tracker.is_double_click(mouse);
            let position = Position::new(mouse.column, mouse.row);
            if ui_areas.notification.contains(position)
                && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            {
                if let Some(url) = components::notification::action_url_at(
                    ui_areas.notification,
                    mouse.column,
                    mouse.row,
                    &ctx,
                ) {
                    open_external_url(&url);
                }
                ctx.dismiss_notification();
            } else if ui_areas.tabs.contains(position)
                && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            {
                if let Some(tab) = pages::sidebar::hit_test(ui_areas.tabs, position) {
                    active_tab = tab;
                    if tab == NavTab::Search {
                        search_page.lock().unwrap().input_mode = true;
                    }
                }
            } else if ui_areas.progress.contains(position)
                && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            {
                let duration = *ctx.duration.borrow();
                if !duration.is_zero() && ui_areas.progress.width > 0 {
                    let offset = mouse.column.saturating_sub(ui_areas.progress.x);
                    let ratio = f64::from(offset) / f64::from(ui_areas.progress.width);
                    ctx.player.seek(Duration::from_secs_f64(
                        duration.as_secs_f64() * ratio.clamp(0.0, 1.0),
                    ));
                }
            } else if ui_areas.content.contains(position) {
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) {
                    let target = match active_tab {
                        NavTab::Main => main_page
                            .context_song_at(mouse, ui_areas.content, &ctx)
                            .map(|target| (target, SongMenuKind::Queue)),
                        NavTab::Search => search_page
                            .lock()
                            .unwrap()
                            .context_song_at(mouse, ui_areas.content)
                            .map(|target| (target, SongMenuKind::Standard)),
                        NavTab::Leaderboard => leaderboard
                            .context_song_at(mouse, ui_areas.content)
                            .map(|target| (target, SongMenuKind::Standard)),
                        NavTab::Playlists => playlists
                            .context_song_at(mouse, ui_areas.content)
                            .map(|target| (target, SongMenuKind::Standard)),
                        NavTab::Favorites => favorites_page
                            .context_song_at(mouse, ui_areas.content, &ctx)
                            .map(|target| (target, SongMenuKind::Standard)),
                        NavTab::History => pages::history::context_song_at(
                            mouse,
                            ui_areas.content,
                            &ctx,
                            &mut history_selected,
                            history_scroll,
                        )
                        .map(|target| (target, SongMenuKind::Standard)),
                        NavTab::LocalMusic => pages::local_music::context_song_at(
                            mouse,
                            ui_areas.content,
                            &ctx,
                            &mut local_selected,
                            local_scroll,
                        )
                        .map(|target| (target, SongMenuKind::Local)),
                        NavTab::Settings => None,
                    };
                    if let Some(((songs, index), kind)) = target {
                        let is_favorite = ctx.storage.is_favorite(&songs[index]);
                        song_menu = SongContextMenu::new(position, songs, index, kind, is_favorite);
                        needs_render = true;
                        continue;
                    }
                }

                let action = match active_tab {
                    NavTab::Main => main_page.handle_mouse(mouse, ui_areas.content, &ctx, activate),
                    NavTab::Search => {
                        search_page
                            .lock()
                            .unwrap()
                            .handle_mouse(mouse, ui_areas.content, activate)
                    }
                    NavTab::Leaderboard => {
                        leaderboard.handle_mouse(mouse, ui_areas.content, activate, &ctx)
                    }
                    NavTab::Playlists => {
                        playlists.handle_mouse(mouse, ui_areas.content, activate, &ctx)
                    }
                    NavTab::Favorites => {
                        favorites_page.handle_mouse(mouse, ui_areas.content, &ctx, activate)
                    }
                    NavTab::History => pages::history::handle_mouse(
                        mouse,
                        ui_areas.content,
                        &ctx,
                        &mut history_selected,
                        history_scroll,
                        activate,
                    ),
                    NavTab::Settings => settings_page.lock().unwrap().handle_mouse(
                        mouse,
                        ui_areas.content,
                        &ctx,
                        &kb_resolver,
                    ),
                    NavTab::LocalMusic => pages::local_music::handle_mouse(
                        mouse,
                        ui_areas.content,
                        &ctx,
                        &mut local_selected,
                        local_scroll,
                        activate,
                    ),
                };

                execute_action(
                    action,
                    &ctx,
                    rt,
                    &action_tx,
                    &search_page,
                    &settings_page,
                    &search_seq,
                );
            }
            needs_render = true;
        } else if matches!(terminal_event, Some(Event::Resize(_, _))) {
            needs_render = true;
        }

        if active_tab == NavTab::Leaderboard {
            maybe_spawn_leaderboard_load(
                &mut leaderboard,
                &mut leaderboard_request_id,
                Arc::clone(&ctx.source_manager),
                leaderboard_tx.clone(),
                rt,
            );
        }
        if active_tab == NavTab::Playlists {
            playlists.sync_favorites(&ctx);
            maybe_spawn_playlist_load(
                &mut playlists,
                &mut playlist_request_id,
                Arc::clone(&ctx.source_manager),
                playlist_tx.clone(),
                rt,
            );
        }

        // === 3. 当前事件未提前 continue 时立即渲染 ===
        if needs_render {
            draw_app(
                terminal,
                &ctx,
                active_tab,
                &search_page,
                &settings_page,
                &mut main_page,
                &mut leaderboard,
                &mut playlists,
                &mut favorites_page,
                &mut history_selected,
                &mut history_scroll,
                &mut local_selected,
                &mut local_scroll,
                &mut ui_areas,
                &confirm_delete,
                &song_menu,
                &bili_login_page,
            )?;
            needs_render = false;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_app(
    terminal: &mut DefaultTerminal,
    ctx: &AppContext,
    active_tab: NavTab,
    search_page: &Arc<std::sync::Mutex<pages::search::SearchPage>>,
    settings_page: &Arc<std::sync::Mutex<pages::settings::SettingsPage>>,
    main_page: &mut pages::main_page::MainPage,
    leaderboard: &mut pages::leaderboard::LeaderboardPage,
    playlists: &mut pages::playlists::PlaylistsPage,
    favorites_page: &mut pages::favorites::FavoritesPage,
    history_selected: &mut usize,
    history_scroll: &mut usize,
    local_selected: &mut usize,
    local_scroll: &mut usize,
    ui_areas: &mut UiAreas,
    confirm_delete: &Option<LocalDeleteConfirmation>,
    song_menu: &Option<SongContextMenu>,
    bili_login_page: &Option<Arc<std::sync::Mutex<pages::bili_login::BiliLoginPage>>>,
) -> anyhow::Result<()> {
    // Kitty 图片是终端外部图层，必须在绘制非主页前清除，避免它短暂覆盖本地/历史页面。
    if active_tab != NavTab::Main {
        ctx.cover_service.clear_display();
    }
    // 封面位置每帧重新记录，先清零，避免窗口过窄不画封面时沿用上一帧的区域
    ctx.cover_service.set_display_area(Rect::ZERO);
    terminal.draw(|frame| {
        let area = frame.area();
        frame.render_widget(
            ratatui::widgets::Block::default().style(
                Style::new()
                    .bg(crate::theme::base(ctx))
                    .fg(crate::theme::text(ctx)),
            ),
            area,
        );
        let main_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4), // header
                Constraint::Length(3), // tabs
                Constraint::Min(3),    // tab content
                Constraint::Length(1), // progress bar
                Constraint::Length(1), // status bar
            ])
            .split(area);

        components::header::render(main_chunks[0], frame.buffer_mut(), ctx);
        pages::sidebar::render(main_chunks[1], frame.buffer_mut(), active_tab, ctx);
        let content_area = main_chunks[2];
        *ui_areas = UiAreas {
            tabs: main_chunks[1],
            content: content_area,
            progress: main_chunks[3],
            notification: Rect::default(),
        };

        match active_tab {
            NavTab::Search => {
                let mut sp = search_page.lock().unwrap();
                sp.render(content_area, frame.buffer_mut(), ctx);
            }
            NavTab::Main => {
                main_page.render(content_area, frame.buffer_mut(), ctx);
            }
            NavTab::Leaderboard => {
                leaderboard.render(content_area, frame.buffer_mut(), ctx);
            }
            NavTab::Playlists => {
                playlists.render(content_area, frame.buffer_mut(), ctx);
            }
            NavTab::Favorites => {
                favorites_page.render(content_area, frame.buffer_mut(), ctx);
            }
            NavTab::History => {
                pages::history::render(
                    content_area,
                    frame.buffer_mut(),
                    ctx,
                    history_selected,
                    history_scroll,
                );
            }
            NavTab::Settings => {
                let mut sp = settings_page.lock().unwrap();
                sp.render(content_area, frame.buffer_mut(), ctx);
            }
            NavTab::LocalMusic => {
                use ratatui::style::{Color, Style};
                use ratatui::text::{Line, Span};
                use ratatui::widgets::{Block, Borders, Paragraph, Widget};

                'local_content: {
                    let local_src = ctx.source_manager.local_source();
                    let paths = ctx.config.read().unwrap().local_music.paths.clone();
                    let songs = local_src.all_songs();
                    let is_scanning = local_src.is_scanning();

                    let block = Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::new().fg(crate::theme::muted(ctx)))
                        .title(if is_scanning {
                            format!("本地音乐 ({} 首，扫描中)", songs.len())
                        } else {
                            format!("本地音乐 ({} 首)", songs.len())
                        });
                    let inner = block.inner(content_area);
                    block.render(content_area, frame.buffer_mut());

                    if inner.height < 2 {
                        break 'local_content;
                    }

                    if paths.is_empty() {
                        Paragraph::new(Line::from(" 未配置音乐目录，请在设置（8）中添加"))
                            .style(Style::new().fg(Color::DarkGray))
                            .render(inner, frame.buffer_mut());
                        break 'local_content;
                    }

                    if songs.is_empty() && is_scanning {
                        Paragraph::new(Line::from(" 正在扫描本地音乐，请稍候..."))
                            .style(Style::new().fg(Color::DarkGray))
                            .render(inner, frame.buffer_mut());
                        break 'local_content;
                    }

                    if songs.is_empty() {
                        Paragraph::new(Line::from(" 目录下未找到音频文件，按 r 重新扫描"))
                            .style(Style::new().fg(Color::DarkGray))
                            .render(inner, frame.buffer_mut());
                        break 'local_content;
                    }

                    let header = pages::components::song_table::header(inner.width);
                    Paragraph::new(Line::from(Span::styled(
                        header,
                        Style::new()
                            .fg(crate::theme::text(ctx))
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )))
                    .render(
                        Rect::new(inner.x, inner.y, inner.width, 1),
                        frame.buffer_mut(),
                    );

                    if inner.height < 3 {
                        break 'local_content;
                    }

                    let visible_height = (inner.height.saturating_sub(2)) as usize;
                    let sel = *local_selected;
                    let mut sc = *local_scroll;

                    if sel >= sc + visible_height {
                        sc = sel.saturating_sub(visible_height.saturating_sub(1));
                    } else if sel < sc {
                        sc = sel;
                    }
                    sc = sc.min(songs.len().saturating_sub(visible_height));
                    *local_scroll = sc;

                    let end = (sc + visible_height).min(songs.len());
                    for (i, song) in songs.iter().enumerate().take(end).skip(sc) {
                        let row = i - sc;
                        let text = pages::components::song_table::row(song, i, inner.width);
                        let line_area =
                            Rect::new(inner.x, inner.y + 1 + row as u16, inner.width, 1);
                        let style = if i == sel {
                            Style::new()
                                .bg(crate::theme::accent(ctx))
                                .fg(crate::theme::selection_fg(ctx))
                        } else {
                            Style::new().fg(crate::theme::text(ctx))
                        };
                        Paragraph::new(Line::from(Span::styled(text, style)))
                            .render(line_area, frame.buffer_mut());
                    }

                    if let Some(confirmation) = confirm_delete {
                        use ratatui::widgets::{Clear, Wrap};
                        let dialog_w = inner.width.saturating_sub(2).min(72);
                        let dialog_h = inner.height.min(7);
                        let dialog_x = inner.x + (inner.width.saturating_sub(dialog_w)) / 2;
                        let dialog_y = inner.y + (inner.height.saturating_sub(dialog_h)) / 2;
                        let dialog_area = Rect::new(dialog_x, dialog_y, dialog_w, dialog_h);
                        Clear.render(dialog_area, frame.buffer_mut());
                        let block = Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::new().fg(crate::theme::rosewater(ctx)))
                            .title("确认删除本地文件");
                        let inner_dialog = block.inner(dialog_area);
                        block.render(dialog_area, frame.buffer_mut());
                        Paragraph::new(vec![
                            Line::from(Span::styled(
                                format!("删除「{}」？", confirmation.name),
                                Style::new().fg(crate::theme::rosewater(ctx)),
                            )),
                            Line::from(Span::styled(
                                confirmation.path.display().to_string(),
                                Style::new().fg(crate::theme::muted(ctx)),
                            )),
                            Line::from(""),
                            Line::from(Span::styled(
                                "y 确认删除    n / Esc 取消",
                                Style::new().fg(crate::theme::text(ctx)),
                            )),
                        ])
                        .wrap(Wrap { trim: false })
                        .render(inner_dialog, frame.buffer_mut());
                    }
                }
            }
        }

        if let Some(page) = bili_login_page {
            use ratatui::widgets::{Clear, Widget};

            let overlay_area = calculate_bili_login_area(area);
            Clear.render(overlay_area, frame.buffer_mut());
            ratatui::widgets::Block::default()
                .style(
                    Style::new()
                        .bg(crate::theme::base(ctx))
                        .fg(crate::theme::text(ctx)),
                )
                .render(overlay_area, frame.buffer_mut());
            let p = page.lock().unwrap();
            p.render(overlay_area, frame.buffer_mut());
        }

        components::progress_bar::render(main_chunks[3], frame.buffer_mut(), ctx);
        components::status_bar::render(main_chunks[4], frame.buffer_mut(), ctx);
        ui_areas.notification = components::notification::area(area, ctx).unwrap_or_default();
        components::notification::render(area, frame.buffer_mut(), ctx);
        if let Some(menu) = song_menu {
            menu.render(content_area, frame.buffer_mut(), ctx);
        }
    })?;
    // 在 Kitty 终端中显示封面（draw 之后，浮动在 TUI 上方）
    if active_tab == NavTab::Main {
        ctx.cover_service.display_kitty();
    }
    Ok(())
}

fn calculate_bili_login_area(area: Rect) -> Rect {
    let qr_width = 66u16;
    let qr_height = 40u16;
    let w = qr_width.min(area.width.saturating_sub(4));
    let h = qr_height.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

#[cfg(target_os = "linux")]
fn current_mpris_snapshot(ctx: &AppContext) -> mpris::MprisSnapshot {
    let song = ctx.current_song.read().unwrap();
    mpris::MprisSnapshot::new(
        *ctx.player_state.borrow(),
        song.as_ref(),
        *ctx.position.borrow(),
        *ctx.duration.borrow(),
        ctx.player.volume(),
        ctx.playlist.mode(),
        ctx.playlist.len(),
    )
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn execute_mpris_command(
    command: mpris::MprisCommand,
    ctx: &AppContext,
    rt: &tokio::runtime::Runtime,
    action_tx: &mpsc::UnboundedSender<AppAction>,
    search_page: &Arc<std::sync::Mutex<pages::search::SearchPage>>,
    settings_page: &Arc<std::sync::Mutex<pages::settings::SettingsPage>>,
    search_seq: &Arc<AtomicU64>,
) -> bool {
    use mpris::MprisCommand;

    let play_entry = |entry: Option<(Vec<SongInfo>, usize)>| {
        if let Some((songs, index)) = entry {
            execute_action(
                AppAction::PlaySong { songs, index },
                ctx,
                rt,
                action_tx,
                search_page,
                settings_page,
                search_seq,
            );
        }
    };

    match command {
        MprisCommand::Quit => return true,
        MprisCommand::Play => ctx.player.resume(),
        MprisCommand::Pause => ctx.player.pause(),
        MprisCommand::Toggle => ctx.player.toggle(),
        MprisCommand::Stop => ctx.player.stop(),
        MprisCommand::Next => play_entry(ctx.playlist.next_manual_entry()),
        MprisCommand::Previous => play_entry(ctx.playlist.prev_manual_entry()),
        MprisCommand::SeekBy(offset) => {
            let current = ctx.position.borrow().as_micros();
            let target = if offset >= 0 {
                current.saturating_add(offset as u128)
            } else {
                current.saturating_sub(offset.unsigned_abs() as u128)
            };
            ctx.player
                .seek(Duration::from_micros(target.min(u64::MAX as u128) as u64));
        }
        MprisCommand::SetPosition(position) => ctx.player.seek(position),
        MprisCommand::SetVolume(volume) => {
            ctx.player
                .set_volume((volume.clamp(0.0, 1.0) * 100.0).round() as u32);
        }
    }
    false
}

/// 执行一个 AppAction（简化版，不再处理 Navigate/GoBack）
fn execute_action(
    action: AppAction,
    ctx: &AppContext,
    rt: &tokio::runtime::Runtime,
    action_tx: &mpsc::UnboundedSender<AppAction>,
    search_page: &Arc<std::sync::Mutex<pages::search::SearchPage>>,
    settings_page: &Arc<std::sync::Mutex<pages::settings::SettingsPage>>,
    search_seq: &Arc<AtomicU64>,
) {
    match action {
        AppAction::Search { keyword, source } => {
            let mut sp = search_page.lock().unwrap();
            sp.begin_search(&keyword, false);
            drop(sp);
            let sp_clone = Arc::clone(search_page);
            spawn_search(
                keyword,
                1,
                false,
                source,
                sp_clone,
                Arc::clone(&ctx.source_manager),
                action_tx.clone(),
                rt,
                search_seq.clone(),
            );
        }
        AppAction::SearchMore {
            keyword,
            page,
            source,
        } => {
            let mut sp = search_page.lock().unwrap();
            if sp.is_searching
                || sp.result_keyword != keyword
                || sp.source_filter != source
                || page != sp.current_page + 1
            {
                return;
            }
            sp.begin_search(&keyword, true);
            drop(sp);
            spawn_search(
                keyword,
                page,
                true,
                source,
                Arc::clone(search_page),
                Arc::clone(&ctx.source_manager),
                action_tx.clone(),
                rt,
                search_seq.clone(),
            );
        }
        AppAction::PlaySong { songs, index } => {
            if let Some(song) = songs.get(index).cloned() {
                ctx.playlist.set_playlist(songs, index);
                ctx.play_attempted_sources.lock().unwrap().clear();
                start_song_playback(song, true, ctx, rt, action_tx);
            }
        }
        AppAction::AddToQueue { song, position } => {
            let song = *song;
            let inserted = ctx.playlist.insert(song.clone(), position);
            let message = match (position, inserted) {
                (InsertPosition::Next, 0) | (InsertPosition::End, _) => {
                    format!("已加入队列: {} - {}", song.name, song.singer)
                }
                (InsertPosition::Next, _) => {
                    format!("下一首播放: {} - {}", song.name, song.singer)
                }
            };
            ctx.notify(Notification::success(message));
        }
        AppAction::RetrySong { song } => {
            start_song_playback(*song, false, ctx, rt, action_tx);
        }
        AppAction::ShowNotification(n) => {
            ctx.notify(n);
        }
        AppAction::ImportSource(url) => {
            tracing::info!("importing JS source: {url}");
            let source_mgr = Arc::clone(&ctx.source_manager);
            let generation = source_mgr.begin_js_source_request(false);
            let default_source = ctx
                .config
                .read()
                .unwrap()
                .source
                .default
                .as_str()
                .to_string();
            let tx = action_tx.clone();

            rt.spawn(async move {
                match lx_source::js::loader::load_source_approving_update(&url, &default_source)
                    .await
                {
                    Ok(source) => {
                        if !source_mgr.set_js_source_if_current(generation, Arc::new(source)) {
                            return;
                        }
                        let _ = tx.send(AppAction::SourceImported { url, generation });
                    }
                    Err(e) => {
                        let _ = tx.send(AppAction::SourceImportFailed {
                            error: e,
                            generation,
                        });
                    }
                }
            });
        }
        AppAction::SourceImported { url, generation } => {
            if !ctx.source_manager.is_js_source_request_current(generation) {
                return;
            }
            tracing::info!("JS source imported: {url}");
            let mut sp = settings_page.lock().unwrap();
            sp.selected_source = 0;
            sp.status_msg = Some("✓ 音源已加载并启用".to_string());
            drop(sp);
            let save_result = {
                let mut config = ctx.config.write().unwrap();
                config.source.js_sources.retain(|item| item != &url);
                config.source.js_sources.insert(0, url);
                crate::config::loader::save(&config, &ctx.config_path)
            };
            if let Err(e) = save_result {
                let mut sp = settings_page.lock().unwrap();
                sp.status_msg = Some(format!("✗ 音源已启用，但保存配置失败: {}", e));
                ctx.notify(Notification::error(format!("保存 JS 音源配置失败: {}", e)));
            } else {
                ctx.notify(Notification::success("JS 音源已加载并启用"));
            }
        }
        AppAction::SourceImportFailed { error, generation } => {
            if !ctx.source_manager.is_js_source_request_current(generation) {
                return;
            }
            tracing::warn!("JS source import failed: {error}");
            let mut sp = settings_page.lock().unwrap();
            sp.status_msg = Some(format!("✗ 音源加载失败: {}", error));
            ctx.notify(Notification::error(format!("JS 音源导入失败: {}", error)));
        }
        AppAction::RemoveSource(url) => {
            tracing::info!("removing JS source: {url}");
            let generation = ctx.source_manager.begin_js_source_request(true);
            let (remaining_urls, default_source) = {
                let mut config = ctx.config.write().unwrap();
                config.source.js_sources.retain(|u| u != &url);
                let remaining_urls = config.source.js_sources.clone();
                let default_source = config.source.default.as_str().to_string();
                if let Err(e) = crate::config::loader::save(&config, &ctx.config_path) {
                    tracing::warn!("保存配置失败: {}", e);
                }
                (remaining_urls, default_source)
            };
            spawn_js_source_loader(
                remaining_urls,
                default_source,
                Arc::clone(&ctx.source_manager),
                generation,
                action_tx.clone(),
                rt,
            );
            let _ = action_tx.send(AppAction::ShowNotification(Notification::success(
                "已移除音源",
            )));
        }
        AppAction::ScanLocalMusic { paths, max_depth } => {
            let generation = next_generation(&ctx.local_scan_request_id);
            let request_seq = Arc::clone(&ctx.local_scan_request_id);
            let local_source = ctx.source_manager.local_source();
            let source_generation = local_source.begin_scan();
            let settings = Arc::clone(settings_page);
            let tx = action_tx.clone();
            rt.spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    let errors =
                        local_source.scan_for_generation(&paths, max_depth, source_generation);
                    let count = local_source.all_songs().len();
                    (errors, count)
                })
                .await;
                if request_seq.load(Ordering::SeqCst) != generation {
                    return;
                }
                let (errors, count) = match result {
                    Ok(result) => result,
                    Err(error) => (vec![format!("本地音乐扫描任务失败: {error}")], 0),
                };
                let mut settings = settings.lock().unwrap();
                if errors.is_empty() {
                    settings.status_msg = Some(format!("扫描完成，共 {} 首", count));
                    let _ = tx.send(AppAction::ShowNotification(Notification::success(format!(
                        "本地音乐扫描完成，共 {} 首",
                        count
                    ))));
                } else {
                    settings.status_msg = Some(format!("扫描错误: {}", errors.join("; ")));
                    for error in errors {
                        let _ = tx.send(AppAction::ShowNotification(Notification::error(error)));
                    }
                }
            });
        }
        AppAction::Navigate(_)
        | AppAction::GoBack
        | AppAction::Quit
        | AppAction::None
        | AppAction::BiliLogin
        | AppAction::BiliLogout
        | AppAction::BiliLoginSuccess => {
            // handled elsewhere or ignored
        }
    }
}

fn start_song_playback(
    song: SongInfo,
    add_history: bool,
    ctx: &AppContext,
    rt: &tokio::runtime::Runtime,
    action_tx: &mpsc::UnboundedSender<AppAction>,
) {
    let request_id = ctx.play_request_id.fetch_add(1, Ordering::SeqCst) + 1;
    let player_generation = ctx.player.prepare();
    let lyric_generation = ctx.lyric_service.prepare();
    let (show_cover, album_cover_notification, track_change_notification) = {
        let config = ctx.config.read().unwrap();
        (
            config.ui.show_cover,
            config.notification.album_cover,
            config.notification.track_change,
        )
    };
    let cover_service = Arc::clone(&ctx.cover_service);
    // 队列里的 SongInfo 通常已经带了封面地址，先用它加载一次，不必等播放地址解析完。
    let initial_cover = song.cover_url.clone();
    if show_cover {
        cover_service.clear();
        let initial_cover = initial_cover.clone();
        let cover = Arc::clone(&cover_service);
        let wake_tx = action_tx.clone();
        rt.spawn(async move {
            if let Err(error) = cover.load(initial_cover).await {
                tracing::debug!("load initial cover failed: {}", error);
            }
            // 封面加载不会产生任何事件，暂停时必须主动唤醒渲染循环
            let _ = wake_tx.send(AppAction::None);
        });
    } else {
        cover_service.clear();
    }

    if add_history {
        ctx.storage.add_history(&song);
    }
    *ctx.current_song.write().unwrap() = Some(song.clone());
    if add_history {
        let _ = action_tx.send(AppAction::ShowNotification(
            Notification::info(format!("正在加载: {} - {}", song.name, song.singer)).tui_only(),
        ));
    }

    let source_mgr = Arc::clone(&ctx.source_manager);
    let player = Arc::clone(&ctx.player);
    let lyric_service = Arc::clone(&ctx.lyric_service);
    let lyric_song = song.clone();
    let lyric_position = ctx.position.clone();
    let lyric_tx = action_tx.clone();
    rt.spawn(async move {
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            lyric_service.load(&lyric_song, lyric_generation),
        )
        .await;
        match result {
            Err(error) => tracing::warn!("load lyric timeout: {}", error),
            Ok(Err(error)) => tracing::warn!("load lyric failed: {}", error),
            Ok(Ok(())) => {}
        }
        lyric_service.update_position(*lyric_position.borrow());
        let _ = lyric_tx.send(AppAction::None);
    });

    let current_song = Arc::clone(&ctx.current_song);
    let play_request_id = Arc::clone(&ctx.play_request_id);
    let attempted_sources = Arc::clone(&ctx.play_attempted_sources);
    let quality = ctx.config.read().unwrap().player.quality;
    let auto_toggle = ctx.config.read().unwrap().source.auto_toggle;
    let tx = action_tx.clone();

    rt.spawn(async move {
        let resolved = tokio::time::timeout(
            Duration::from_secs(40),
            resolve_playable_song(
                Arc::clone(&source_mgr),
                song,
                quality,
                auto_toggle,
                Arc::clone(&play_request_id),
                Arc::clone(&attempted_sources),
                request_id,
            ),
        )
        .await;

        if play_request_id.load(Ordering::SeqCst) != request_id {
            return;
        }

        let (mut resolved_song, song_url) = match resolved {
            Ok(Ok(Some(resolved))) => resolved,
            Ok(Ok(None)) => return,
            Ok(Err(error)) => {
                player.stop();
                let _ = tx.send(AppAction::ShowNotification(Notification::error(error)));
                return;
            }
            Err(_) => {
                player.stop();
                let _ = tx.send(AppAction::ShowNotification(Notification::error(
                    "获取播放地址超时，请稍后重试",
                )));
                return;
            }
        };

        let url = song_url.url;
        let headers = song_url.headers;
        let player_for_start = Arc::clone(&player);
        let request_guard = Arc::clone(&play_request_id);
        let accepted = tokio::task::spawn_blocking(move || {
            if request_guard.load(Ordering::SeqCst) != request_id {
                return false;
            }
            player_for_start.play_with_headers(&url, player_generation, &headers)
        })
        .await
        .unwrap_or(false);
        if !accepted || play_request_id.load(Ordering::SeqCst) != request_id {
            return;
        }

        if resolved_song.cover_url.is_none() {
            resolved_song.cover_url = song_url.cover_url.clone();
        }
        if resolved_song.cover_url.is_none()
            && let Ok(Ok(url)) = tokio::time::timeout(
                Duration::from_secs(10),
                source_mgr.get_cover_url(&resolved_song),
            )
            .await
        {
            resolved_song.cover_url = Some(url);
        }
        *current_song.write().unwrap() = Some(resolved_song.clone());
        let playing_message = format!(
            "{} - {} [{}]",
            resolved_song.name,
            resolved_song.singer,
            resolved_song.source.as_str()
        );
        let playing_title = format!("正在播放: {}", resolved_song.name);
        let _ = tx.send(AppAction::ShowNotification(
            Notification::info(playing_message.clone())
                .with_title(playing_title.clone())
                .tui_only(),
        ));

        // - 解析结果无封面时不加载
        // - 解析后的封面地址跟队列里的相同时不再重复加载
        if show_cover
            && resolved_song.cover_url.is_some()
            && resolved_song.cover_url != initial_cover
        {
            if let Err(error) = cover_service.load(resolved_song.cover_url.clone()).await {
                tracing::debug!("load cover failed: {}", error);
            }
            let _ = tx.send(AppAction::None);
        }

        let notification_icon = if album_cover_notification {
            match cover_service
                .cache_path(resolved_song.cover_url.clone())
                .await
            {
                Ok(path) => path,
                Err(error) => {
                    tracing::debug!("cache notification cover failed: {error}");
                    None
                }
            }
        } else {
            None
        };
        if track_change_notification {
            let mut notification = Notification::info(playing_message)
                .with_title(playing_title)
                .replacing_previous()
                .desktop_only();
            if let Some(icon) = notification_icon {
                notification = notification.with_icon(icon);
            }
            let _ = tx.send(AppAction::ShowNotification(notification));
        }
    });
}

async fn resolve_playable_song(
    source_manager: Arc<lx_source::manager::SourceManager>,
    song: SongInfo,
    quality: Quality,
    auto_toggle: bool,
    play_request_id: Arc<AtomicU64>,
    attempted_sources: Arc<std::sync::Mutex<std::collections::HashSet<SourceId>>>,
    request_id: u64,
) -> Result<Option<(SongInfo, SongUrl)>, String> {
    let direct_error = if mark_source_attempted(&attempted_sources, song.source) {
        match source_manager.get_song_url(&song, quality).await {
            Ok(url) => return Ok(Some((song, url))),
            Err(error) => error.to_string(),
        }
    } else {
        format!("音源 {} 已尝试", song.source.as_str())
    };

    if play_request_id.load(Ordering::SeqCst) != request_id {
        return Ok(None);
    }
    if !auto_toggle {
        return Err(format!("获取播放地址失败: {}", direct_error));
    }

    let candidates = source_manager.find_music(&song).await;
    if play_request_id.load(Ordering::SeqCst) != request_id {
        return Ok(None);
    }

    for candidate in candidates {
        if !mark_source_attempted(&attempted_sources, candidate.source) {
            continue;
        }
        match source_manager.get_song_url(&candidate, quality).await {
            Ok(url) => return Ok(Some((candidate, url))),
            Err(error) => {
                tracing::debug!(
                    "toggle source failed for {} [{}]: {}",
                    candidate.name,
                    candidate.source.as_str(),
                    error
                );
            }
        }
        if play_request_id.load(Ordering::SeqCst) != request_id {
            return Ok(None);
        }
    }

    Err(format!(
        "获取播放地址失败，换源后仍不可用: {}",
        direct_error
    ))
}

fn mark_source_attempted(
    attempted_sources: &std::sync::Mutex<std::collections::HashSet<SourceId>>,
    source: SourceId,
) -> bool {
    attempted_sources.lock().unwrap().insert(source)
}

fn spawn_js_source_loader(
    urls: Vec<String>,
    default_source: String,
    source_manager: Arc<lx_source::manager::SourceManager>,
    generation: u64,
    tx: mpsc::UnboundedSender<AppAction>,
    rt: &tokio::runtime::Runtime,
) {
    let urls: Vec<String> = urls
        .into_iter()
        .filter(|url| !url.trim().is_empty())
        .collect();
    if urls.is_empty() {
        source_manager.clear_js_source_if_current(generation);
        return;
    }

    rt.spawn(async move {
        let mut last_error = None;
        for url in urls {
            match lx_source::js::loader::load_source(&url, &default_source).await {
                Ok(source) => {
                    if !source_manager.set_js_source_if_current(generation, Arc::new(source)) {
                        return;
                    }
                    let _ = tx.send(AppAction::ShowNotification(Notification::success(
                        "JS 音源已就绪",
                    )));
                    return;
                }
                Err(error) => {
                    if !source_manager.is_js_source_request_current(generation) {
                        return;
                    }
                    tracing::warn!("load JS source failed ({}): {}", url, error);
                    last_error = Some(error);
                }
            }
        }

        if !source_manager.clear_js_source_if_current(generation) {
            return;
        }
        if let Some(error) = last_error {
            let _ = tx.send(AppAction::ShowNotification(Notification::error(format!(
                "没有可用的 JS 音源: {}",
                error
            ))));
        }
    });
}

fn next_generation(sequence: &AtomicU64) -> u64 {
    sequence.fetch_add(1, Ordering::SeqCst) + 1
}

fn should_scan_local_music_on_entry(
    previous_tab: NavTab,
    active_tab: NavTab,
    enabled: bool,
    has_paths: bool,
    songs_empty: bool,
    is_scanning: bool,
) -> bool {
    previous_tab != NavTab::LocalMusic
        && active_tab == NavTab::LocalMusic
        && enabled
        && has_paths
        && songs_empty
        && !is_scanning
}

fn previous_list_index(selected: usize, len: usize, wrap: bool) -> usize {
    match (selected, len, wrap) {
        (_, 0, _) => 0,
        (0, len, true) => len - 1,
        _ => selected.saturating_sub(1).min(len - 1),
    }
}

fn next_list_index(selected: usize, len: usize, wrap: bool) -> usize {
    match len {
        0 => 0,
        _ if selected + 1 < len => selected + 1,
        _ if wrap => 0,
        _ => len - 1,
    }
}

/// 异步搜索（直接 async，不用 spawn_blocking——reqwest 是真正 async 的）
#[allow(clippy::too_many_arguments)]
fn spawn_search(
    keyword: String,
    page: u32,
    append: bool,
    source: Option<lx_core::model::source::SourceId>,
    search_page: Arc<std::sync::Mutex<pages::search::SearchPage>>,
    source_manager: Arc<lx_source::manager::SourceManager>,
    tx: mpsc::UnboundedSender<AppAction>,
    rt: &tokio::runtime::Runtime,
    seq: Arc<AtomicU64>,
) {
    let my_seq = seq.fetch_add(1, Ordering::SeqCst);
    rt.spawn(async move {
        let result = tokio::time::timeout(
            Duration::from_secs(12),
            source_manager.search_scoped(&keyword, page, 30, source),
        )
        .await;
        match result {
            Ok(Ok(search_result)) => {
                if seq.load(Ordering::SeqCst) != my_seq + 1 {
                    return;
                }
                let mut sp = search_page.lock().unwrap();
                sp.update_results(keyword, page, append, search_result, source);
                let _ = tx.send(AppAction::None);
            }
            Ok(Err(error)) => {
                if seq.load(Ordering::SeqCst) != my_seq + 1 {
                    return;
                }
                let mut sp = search_page.lock().unwrap();
                sp.update_error(error.to_string());
                let _ = tx.send(AppAction::ShowNotification(Notification::error(format!(
                    "搜索失败: {}",
                    error
                ))));
            }
            Err(_) => {
                if seq.load(Ordering::SeqCst) != my_seq + 1 {
                    return;
                }
                let mut sp = search_page.lock().unwrap();
                sp.update_error("请求超时，请稍后重试".to_string());
                let _ = tx.send(AppAction::ShowNotification(Notification::error(
                    "搜索超时，请稍后重试".to_string(),
                )));
            }
        }
    });
}

fn maybe_spawn_leaderboard_load(
    leaderboard: &mut pages::leaderboard::LeaderboardPage,
    request_id: &mut u64,
    source_manager: Arc<lx_source::manager::SourceManager>,
    leaderboard_tx: mpsc::UnboundedSender<LeaderboardResponse>,
    rt: &tokio::runtime::Runtime,
) {
    let Some(request) = leaderboard.next_load_request() else {
        return;
    };
    leaderboard.begin_loading(&request);
    *request_id = request_id.wrapping_add(1);
    spawn_leaderboard_request(*request_id, request, source_manager, leaderboard_tx, rt);
}

/// 异步加载排行榜目录或歌曲。
fn spawn_leaderboard_request(
    request_id: u64,
    request: pages::leaderboard::LeaderboardLoadRequest,
    source_manager: Arc<lx_source::manager::SourceManager>,
    leaderboard_tx: mpsc::UnboundedSender<LeaderboardResponse>,
    rt: &tokio::runtime::Runtime,
) {
    rt.spawn(async move {
        let response = match request {
            pages::leaderboard::LeaderboardLoadRequest::Boards { source } => {
                let result = tokio::time::timeout(
                    Duration::from_secs(12),
                    source_manager.leaderboard_boards(source),
                )
                .await;
                LeaderboardResponse::Boards {
                    request_id,
                    source,
                    result: match result {
                        Ok(Ok(boards)) => Ok(boards),
                        Ok(Err(error)) => Err(error.to_string()),
                        Err(_) => Err("请求超时，请稍后重试".to_string()),
                    },
                }
            }
            pages::leaderboard::LeaderboardLoadRequest::Songs { source, board_id } => {
                let result = tokio::time::timeout(
                    Duration::from_secs(12),
                    source_manager.leaderboard(source, &board_id, 1, 300),
                )
                .await;
                LeaderboardResponse::Songs {
                    request_id,
                    source,
                    board_id,
                    result: match result {
                        Ok(Ok(search_result)) => Ok(search_result.items),
                        Ok(Err(error)) => Err(error.to_string()),
                        Err(_) => Err("请求超时，请稍后重试".to_string()),
                    },
                }
            }
        };
        let _ = leaderboard_tx.send(response);
    });
}

fn maybe_spawn_playlist_load(
    playlists: &mut pages::playlists::PlaylistsPage,
    request_id: &mut u64,
    source_manager: Arc<lx_source::manager::SourceManager>,
    playlist_tx: mpsc::UnboundedSender<PlaylistResponse>,
    rt: &tokio::runtime::Runtime,
) {
    let Some(request) = playlists.next_load_request() else {
        return;
    };
    playlists.begin_loading(&request);
    *request_id = request_id.wrapping_add(1);
    spawn_playlist_request(*request_id, request, source_manager, playlist_tx, rt);
}

fn spawn_playlist_request(
    request_id: u64,
    request: pages::playlists::PlaylistLoadRequest,
    source_manager: Arc<lx_source::manager::SourceManager>,
    playlist_tx: mpsc::UnboundedSender<PlaylistResponse>,
    rt: &tokio::runtime::Runtime,
) {
    rt.spawn(async move {
        let response = match request {
            pages::playlists::PlaylistLoadRequest::List { source } => {
                let result = tokio::time::timeout(
                    Duration::from_secs(12),
                    source_manager.playlists(source, 1),
                )
                .await;
                PlaylistResponse::List {
                    request_id,
                    source,
                    result: match result {
                        Ok(Ok(playlists)) => Ok(playlists),
                        Ok(Err(error)) => Err(error.to_string()),
                        Err(_) => Err("请求超时，请稍后重试".to_string()),
                    },
                }
            }
            pages::playlists::PlaylistLoadRequest::Songs {
                source,
                playlist_id,
            } => {
                let timeout = if source == SourceId::Bili {
                    Duration::from_secs(45)
                } else {
                    Duration::from_secs(15)
                };
                let result = tokio::time::timeout(
                    timeout,
                    source_manager.playlist_detail(source, &playlist_id, 1),
                )
                .await;
                PlaylistResponse::Songs {
                    request_id,
                    source,
                    playlist_id,
                    result: match result {
                        Ok(Ok(songs)) => Ok(songs),
                        Ok(Err(error)) => Err(error.to_string()),
                        Err(_) => Err("请求超时，请稍后重试".to_string()),
                    },
                }
            }
        };
        let _ = playlist_tx.send(response);
    });
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{
        DeleteConfirmationAction, delete_confirmation_action, next_list_index, previous_list_index,
        should_scan_local_music_on_entry,
    };
    use crate::pages::sidebar::NavTab;

    #[test]
    fn local_list_navigation_wraps_at_both_ends() {
        assert_eq!(previous_list_index(0, 4, true), 3);
        assert_eq!(next_list_index(3, 4, true), 0);
        assert_eq!(previous_list_index(0, 4, false), 0);
        assert_eq!(next_list_index(3, 4, false), 3);
    }

    #[test]
    fn local_delete_confirmation_accepts_terminal_shift_variants() {
        for key in [
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE),
        ] {
            assert_eq!(
                delete_confirmation_action(&key),
                DeleteConfirmationAction::Confirm
            );
        }

        for key in [
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        ] {
            assert_eq!(
                delete_confirmation_action(&key),
                DeleteConfirmationAction::Cancel
            );
        }
    }

    #[test]
    fn entering_empty_local_music_page_starts_scan() {
        assert!(should_scan_local_music_on_entry(
            NavTab::History,
            NavTab::LocalMusic,
            true,
            true,
            true,
            false,
        ));
    }

    #[test]
    fn local_music_entry_does_not_start_invalid_or_duplicate_scan() {
        for (enabled, has_paths, songs_empty, is_scanning) in [
            (false, true, true, false),
            (true, false, true, false),
            (true, true, false, false),
            (true, true, true, true),
        ] {
            assert!(!should_scan_local_music_on_entry(
                NavTab::History,
                NavTab::LocalMusic,
                enabled,
                has_paths,
                songs_empty,
                is_scanning,
            ));
        }

        assert!(!should_scan_local_music_on_entry(
            NavTab::LocalMusic,
            NavTab::LocalMusic,
            true,
            true,
            true,
            false,
        ));
    }
}
