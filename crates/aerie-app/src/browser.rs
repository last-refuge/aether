//! Main window: welcome screen, sidebar and the My Drive file browser.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::rc::Rc;

use adw::prelude::*;
use aerie_core::{
    BrowseState, Catalog, DriveFileId, FileEntry, FileKind, MemoryCatalog, compare_names,
    format_size,
};
use aerie_store::Account;
use gtk::{gio, glib};

use crate::catalog::{AppCatalog, StoreCatalog};
use crate::engine::{Command, Engine, Event, LocalFile};
use crate::fixtures::{self, ROOT_ID};
use crate::sync_page::SyncPage;
use crate::transfers_page::TransfersPage;

/// Opens the main window. Drive work happens in `aerie-service`, which keeps syncing after
/// the window closes.
pub fn build_window(app: &adw::Application) {
    if let Some(window) = app.windows().first() {
        window.present();
        return;
    }
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Aerie")
        .default_width(1080)
        .default_height(700)
        .width_request(360)
        .height_request(320)
        .build();

    let (events, inbox) = async_channel::unbounded();
    let engine = Rc::new(Engine::start(events));
    let browser = Browser::new(engine.clone());
    let welcome = Welcome::new(engine.clone());
    let transfers = TransfersPage::new(engine.clone());
    let sync = SyncPage::new(engine.clone());
    let activity = crate::activity_page::ActivityPage::new();
    let split = browser.split_view(&transfers.root, &sync.root, &activity.root);
    sync.view_activity.connect_clicked(glib::clone!(
        #[weak]
        browser,
        move |_| browser.show_page("activity")
    ));

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(180)
        .build();
    // Until the service says who's signed in (or that nobody is), show that Aerie is
    // starting rather than asking to connect: replacing an older service takes a moment.
    stack.add_named(&starting_page(), Some("starting"));
    stack.add_named(&welcome.root, Some("welcome"));
    stack.add_named(&split, Some("browser"));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));
    window.set_content(Some(&toasts));

    let narrow = adw::Breakpoint::new(
        adw::BreakpointCondition::parse("max-width: 640sp").expect("valid breakpoint"),
    );
    narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(narrow);

    install_actions(&window, &stack, &browser, &welcome, &engine);
    // If the service never answers, don't leave a spinner up forever.
    glib::timeout_add_seconds_local_once(
        20,
        glib::clone!(
            #[weak]
            stack,
            move || {
                if stack.visible_child_name().as_deref() == Some("starting") {
                    stack.set_visible_child_name("welcome");
                }
            }
        ),
    );
    welcome.explore.connect_clicked(glib::clone!(
        #[weak]
        stack,
        move |_| stack.set_visible_child_name("browser")
    ));

    glib::spawn_future_local(glib::clone!(
        #[weak]
        window,
        #[weak]
        stack,
        #[weak]
        toasts,
        #[strong]
        browser,
        #[strong]
        welcome,
        #[strong]
        transfers,
        #[strong]
        sync,
        async move {
            while let Ok(event) = inbox.recv().await {
                if matches!(
                    event,
                    Event::TransfersChanged | Event::SignedIn(_) | Event::SignedOut
                ) {
                    let list = browser.transfers();
                    browser.set_transfer_count(TransfersPage::active_count(&list));
                    transfers.show(list);
                }
                if matches!(
                    event,
                    Event::Sync(_) | Event::SignedIn(_) | Event::SignedOut | Event::CatalogChanged
                ) {
                    let (settings, roots, folders) = browser.sync_view();
                    let synced = if settings.as_ref().is_some_and(|s| !s.paused) {
                        roots.clone()
                    } else {
                        Vec::new()
                    };
                    browser.set_synced_roots(synced);
                    sync.show(settings, roots, folders);
                }
                match event {
                    Event::TransferProgress { id, done, total } => {
                        transfers.progress(id, done, total)
                    }
                    Event::TransfersChanged => {}
                    Event::SyncProgress {
                        root,
                        done,
                        total,
                        verb,
                        item,
                    } => {
                        let folder = root.as_ref().and_then(|id| browser.folder_title(id));
                        let progress = crate::sync_page::Progress {
                            done,
                            total,
                            verb,
                            item,
                            folder,
                        };
                        sync.set_progress(&progress);
                        browser.set_sync_progress(Some(&progress));
                        browser.set_sync_activity(root);
                    }
                    Event::SyncActivity(log) => activity.show(log),
                    Event::Sync(state) => {
                        if state != crate::engine::SyncState::Syncing {
                            browser.set_sync_activity(None);
                            browser.set_sync_progress(None);
                        }
                        sync.set_state(&state);
                        browser.set_sync_state(state);
                    }
                    Event::ConflictKept(path) => {
                        let name = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        let message = format!("Kept both versions. Drive’s copy is “{name}”.");
                        toasts.add_toast(adw::Toast::new(&message));
                        notify(&window, "conflict", "Kept both versions", &message);
                    }
                    event => handle_event(event, &window, &stack, &toasts, &browser, &welcome),
                }
            }
        }
    ));

    // Closing the window quits the app; the service carries on syncing. An explicit quit
    // avoids lingering when widgets still hold references to the window.
    window.connect_close_request(|window| {
        if let Some(app) = window.application() {
            app.quit();
        }
        glib::Propagation::Proceed
    });

    window.present();
}

fn install_actions(
    window: &adw::ApplicationWindow,
    stack: &gtk::Stack,
    browser: &Rc<Browser>,
    welcome: &Rc<Welcome>,
    engine: &Rc<Engine>,
) {
    let search = gio::SimpleAction::new("search", None);
    search.connect_activate(glib::clone!(
        #[strong]
        browser,
        #[weak]
        stack,
        move |_, _| {
            if stack.visible_child_name().as_deref() == Some("browser") {
                browser.search.grab_focus();
            }
        }
    ));
    let new_folder = gio::SimpleAction::new("new-folder", None);
    new_folder.connect_activate(glib::clone!(
        #[strong]
        browser,
        move |_, _| browser.show_new_folder_dialog()
    ));
    let sign_in = gio::SimpleAction::new("sign-in", None);
    sign_in.connect_activate(glib::clone!(
        #[strong]
        engine,
        #[strong]
        welcome,
        #[weak]
        stack,
        move |_, _| {
            welcome.set_waiting(true);
            stack.set_visible_child_name("welcome");
            engine.send(Command::SignIn { write: false });
        }
    ));
    let allow_changes = gio::SimpleAction::new("allow-changes", None);
    allow_changes.connect_activate(glib::clone!(
        #[strong]
        engine,
        #[weak]
        window,
        move |_, _| {
            let engine = engine.clone();
            crate::actions::ask_for_write_access(&window, move || {
                engine.send(Command::SignIn { write: true });
            });
        }
    ));
    let rename = gio::SimpleAction::new("rename", None);
    rename.connect_activate(glib::clone!(
        #[strong]
        browser,
        move |_, _| browser.rename_selected()
    ));
    let move_to = gio::SimpleAction::new("move", None);
    move_to.connect_activate(glib::clone!(
        #[strong]
        browser,
        move |_, _| browser.move_selected()
    ));
    let trash = gio::SimpleAction::new("trash", None);
    trash.connect_activate(glib::clone!(
        #[strong]
        browser,
        move |_, _| browser.trash_selected()
    ));
    let upload = gio::SimpleAction::new("upload", None);
    upload.connect_activate(glib::clone!(
        #[strong]
        browser,
        move |_, _| browser.choose_uploads()
    ));
    let download = gio::SimpleAction::new("download", None);
    download.connect_activate(glib::clone!(
        #[strong]
        browser,
        move |_, _| browser.download_selected()
    ));
    let preferences = gio::SimpleAction::new("preferences", None);
    preferences.connect_activate(glib::clone!(
        #[strong]
        browser,
        #[weak]
        window,
        move |_, _| crate::preferences::show(&window, browser.indexing_choice())
    ));
    let open_in_drive = gio::SimpleAction::new("open-in-drive", None);
    open_in_drive.connect_activate(glib::clone!(
        #[strong]
        browser,
        move |_, _| browser.open_selected_in_drive()
    ));
    // A check item in the folder menu: on when the folder is one of the synced folders.
    let sync_folder = gio::SimpleAction::new_stateful("sync-folder", None, &false.to_variant());
    sync_folder.connect_activate(glib::clone!(
        #[strong]
        browser,
        move |_, _| browser.toggle_sync_selected()
    ));
    let sync_root = gio::SimpleAction::new("sync-root", Some(glib::VariantTy::STRING));
    sync_root.connect_activate(glib::clone!(
        #[strong]
        browser,
        move |_, id| {
            if let Some(id) = id.and_then(|id| id.str()) {
                browser.toggle_sync_root_by_id(id);
            }
        }
    ));
    let open_local = gio::SimpleAction::new("open-local-folder", Some(glib::VariantTy::STRING));
    open_local.connect_activate(glib::clone!(
        #[strong]
        browser,
        move |_, id| {
            if let Some(id) = id.and_then(|id| id.str()) {
                browser.open_local_drive(&DriveFileId::new(id));
            }
        }
    ));
    let sign_out = gio::SimpleAction::new("sign-out", None);
    sign_out.set_enabled(false);
    sign_out.connect_activate(glib::clone!(
        #[strong]
        engine,
        move |_, _| engine.send(Command::SignOut)
    ));
    let refresh = gio::SimpleAction::new("refresh", None);
    refresh.connect_activate(glib::clone!(
        #[strong]
        engine,
        move |_, _| engine.send(Command::Refresh)
    ));
    for action in [
        &search,
        &new_folder,
        &sign_in,
        &allow_changes,
        &sign_out,
        &refresh,
        &rename,
        &move_to,
        &trash,
        &upload,
        &download,
        &open_in_drive,
        &sync_folder,
        &sync_root,
        &open_local,
        &preferences,
    ] {
        window.add_action(action);
    }
    browser.update_actions();
}

fn set_action_enabled(window: &adw::ApplicationWindow, name: &str, enabled: bool) {
    if let Some(action) = window
        .lookup_action(name)
        .and_downcast::<gio::SimpleAction>()
    {
        action.set_enabled(enabled);
    }
}

fn handle_event(
    event: Event,
    window: &adw::ApplicationWindow,
    stack: &gtk::Stack,
    toasts: &adw::ToastOverlay,
    browser: &Rc<Browser>,
    welcome: &Rc<Welcome>,
) {
    match event {
        Event::OpenUri(uri) => open_uri(window, &uri),
        Event::SignInStarted => {
            welcome.set_waiting(true);
            if browser.is_drive() {
                toasts.add_toast(adw::Toast::new("Continue in your browser."));
            }
        }
        Event::SignInFailed(message) => {
            welcome.set_waiting(false);
            if browser.is_drive() {
                // A failed upgrade to write access leaves the read-only session in place.
                toasts.add_toast(adw::Toast::new(&message));
            } else {
                welcome.show_error(&message);
                stack.set_visible_child_name("welcome");
            }
        }
        Event::SignedIn(account) => {
            welcome.set_waiting(false);
            browser.show_drive(&account);
            set_action_enabled(window, "sign-out", true);
            show_page(stack, "browser");
        }
        Event::NeedsSignIn { expired } => {
            welcome.set_waiting(false);
            if expired && browser.is_drive() {
                // Keep the cached files on screen and ask for a fresh sign-in.
                browser.set_status(Status::NeedsSignIn);
            } else {
                stack.set_visible_child_name("welcome");
            }
        }
        Event::SignedOut => {
            browser.show_samples();
            set_action_enabled(window, "sign-out", false);
            welcome.show_error("");
            stack.set_visible_child_name("welcome");
        }
        Event::Indexing { files } => browser.set_status(Status::Indexing(files)),
        Event::UpToDate => browser.set_status(Status::UpToDate),
        Event::Storage { used, limit } => browser.set_storage(Some((used, limit))),
        Event::CatalogChanged => browser.reload(),
        Event::Offline => browser.set_status(Status::Offline),
        Event::Problem(message) => toasts.add_toast(adw::Toast::new(&message)),
        Event::FolderLoaded(folder) => browser.folder_loaded(&folder),
        // Remote results were cached; searching the cache again includes them.
        Event::RemoteResults { .. } => browser.reload(),
        Event::TransfersChanged
        | Event::TransferProgress { .. }
        | Event::SyncProgress { .. }
        | Event::SyncActivity(_)
        | Event::Sync(_)
        | Event::ConflictKept(_) => {}
        Event::NeedsWriteAccess => {
            let _ = WidgetExt::activate_action(window, "win.allow-changes", None);
        }
        Event::Done { message, undo } => {
            let toast = adw::Toast::new(&message);
            if let Some(id) = undo {
                toast.set_button_label(Some("Undo"));
                let engine = browser.engine.clone();
                toast.connect_button_clicked(move |_| {
                    engine.send(Command::Untrash { id: id.clone() });
                });
            }
            toasts.add_toast(toast);
        }
    }
}

/// A desktop notification, for things worth knowing while the window is hidden.
fn notify(window: &adw::ApplicationWindow, id: &str, title: &str, body: &str) {
    if let Some(app) = window.application() {
        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        app.send_notification(Some(id), &notification);
    }
}

/// Switches pages, without the crossfade before the window is on screen: a transition
/// started while unmapped can leave the old page showing.
fn starting_page() -> gtk::Widget {
    let spinner = adw::SpinnerPaintable::new(None::<&gtk::Widget>);
    let page = adw::StatusPage::builder()
        .paintable(&spinner)
        .title("Starting Aerie")
        .build();
    spinner.set_widget(Some(&page));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::builder().show_title(false).build());
    view.set_content(Some(&page));
    view.add_css_class("aerie-welcome");
    view.upcast()
}

fn show_page(stack: &gtk::Stack, name: &str) {
    let transition = if stack.is_mapped() {
        stack.transition_type()
    } else {
        gtk::StackTransitionType::None
    };
    stack.set_visible_child_full(name, transition);
}

fn open_uri(window: &adw::ApplicationWindow, uri: &str) {
    gtk::UriLauncher::new(uri).launch(Some(window), gio::Cancellable::NONE, |result| {
        if let Err(err) = result {
            tracing::warn!(%err, "couldn't open the browser");
        }
    });
}

/// The first screen: one sign-in action, with progress and recoverable errors.
struct Welcome {
    root: gtk::Widget,
    connect: gtk::Button,
    explore: gtk::Button,
    waiting: gtk::Box,
    message: gtk::Label,
}

impl Welcome {
    fn new(engine: Rc<Engine>) -> Rc<Self> {
        let connect = gtk::Button::builder()
            .label("Connect Google Drive")
            .halign(gtk::Align::Center)
            .action_name("win.sign-in")
            .css_classes(["aerie-primary"])
            .build();
        let explore = gtk::Button::builder()
            .label("Explore sample files")
            .halign(gtk::Align::Center)
            .css_classes(["aerie-secondary"])
            .build();

        let cancel = gtk::Button::builder()
            .label("Cancel")
            .css_classes(["flat"])
            .build();
        cancel.connect_clicked(move |_| engine.send(Command::CancelSignIn));
        let waiting = gtk::Box::builder()
            .spacing(12)
            .halign(gtk::Align::Center)
            .visible(false)
            .build();
        waiting.append(&adw::Spinner::new());
        waiting.append(&gtk::Label::new(Some("Finish signing in in your browser.")));
        waiting.append(&cancel);

        let message = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["error"])
            .visible(false)
            .build();
        let note = gtk::Label::builder()
            .label(
                "Aerie asks Google for read-only access to your Drive and talks to Google \
                 directly from this computer.",
            )
            .wrap(true)
            .max_width_chars(48)
            .justify(gtk::Justification::Center)
            .css_classes(["metadata"])
            .build();

        let actions = gtk::Box::new(gtk::Orientation::Vertical, 16);
        actions.append(&connect);
        actions.append(&waiting);
        actions.append(&message);
        actions.append(&note);
        actions.append(&explore);

        let mark = gtk::Image::builder()
            .icon_name(concat!("ai.lastrefuge.Aerie", "-mark"))
            .pixel_size(168)
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build();
        let wordmark = gtk::Label::builder()
            .label("Aerie")
            .css_classes(["brand-wordmark"])
            .build();
        let descriptor = gtk::Label::builder()
            .label("GOOGLE DRIVE FOR LINUX")
            .css_classes(["brand-descriptor"])
            .build();
        let tagline = gtk::Label::builder()
            .label("Your files, everywhere.")
            .css_classes(["brand-tagline"])
            .margin_top(12)
            .build();
        let brand = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_bottom(28)
            .build();
        brand.append(&mark);
        brand.append(&wordmark);
        brand.append(&descriptor);
        brand.append(&tagline);
        actions.prepend(&brand);
        let status = adw::StatusPage::builder().child(&actions).build();

        let view = adw::ToolbarView::new();
        let header = adw::HeaderBar::builder().show_title(false).build();
        header.pack_end(&crate::app_menu::button());
        view.add_top_bar(&header);
        view.set_content(Some(&status));
        view.add_css_class("aerie-welcome");
        connect.grab_focus();

        Rc::new(Self {
            root: view.upcast(),
            connect,
            explore,
            waiting,
            message,
        })
    }

    fn set_waiting(&self, waiting: bool) {
        self.waiting.set_visible(waiting);
        self.connect.set_visible(!waiting);
        if waiting {
            self.show_error("");
        }
    }

    fn show_error(&self, message: &str) {
        self.message.set_label(message);
        self.message.set_visible(!message.is_empty());
    }
}

/// How the status badge is tinted.
#[derive(Clone, Copy)]
enum Tone {
    Good,
    Working,
    Warning,
    Off,
}

impl Tone {
    fn class(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Working => "working",
            Self::Warning => "warning",
            Self::Off => "off",
        }
    }
}

#[derive(Clone, Copy)]
enum Status {
    Samples,
    Indexing(u64),
    UpToDate,
    Offline,
    NeedsSignIn,
}

/// Widgets over a [`BrowseState`]. Navigation and search rebuild the visible list.
struct Browser {
    engine: Rc<Engine>,
    /// A folder in an unindexed drive being fetched on demand.
    loading_folder: RefCell<Option<DriveFileId>>,
    synced_roots: RefCell<std::collections::HashSet<DriveFileId>>,
    /// The synced folder the current pass is working in.
    active_root: RefCell<Option<DriveFileId>>,
    /// The mark for the open folder, shown beside its breadcrumb.
    crumb_mark: Cell<SyncMark>,
    /// The shared drive being browsed, or `None` for My Drive.
    tree: RefCell<Option<FileEntry>>,
    /// Folds the shared drives away; shows how many there are.
    drives_header: gtk::Button,
    drives_count: gtk::Label,
    drives_revealer: gtk::Revealer,
    /// Narrows a long list of drives by name.
    drives_filter: gtk::SearchEntry,
    /// Shown under the heading until the first index has found the shared drives.
    drives_loading: gtk::Box,
    drives_list: gtk::ListBox,
    /// Synced badges in the sidebar, for My Drive and each shared drive.
    my_drive_badge: gtk::Image,
    drive_badges: RefCell<Vec<(DriveFileId, gtk::Image)>>,
    /// Shared drives with something syncing: the whole drive or a folder in it.
    synced_drives: RefCell<std::collections::HashSet<DriveFileId>>,
    /// Lists every shared drive, or only the synced ones; remembered.
    synced_only: gtk::ToggleButton,
    /// Says so when the synced-only list is empty.
    no_synced_drives: gtk::Label,
    drives: RefCell<Vec<FileEntry>>,
    places: gtk::ListBox,
    /// The content area: the file list, or the transfer list.
    pages: gtk::Stack,
    transfers_row: gtk::ListBoxRow,
    transfers_badge: gtk::Label,
    sync_row: gtk::ListBoxRow,
    sync_state: RefCell<Option<crate::engine::SyncState>>,
    selection: gtk::SingleSelection,
    state: RefCell<BrowseState<AppCatalog>>,
    account: RefCell<Option<Account>>,
    indexing: Cell<bool>,
    next_local_id: Cell<u32>,
    store: gio::ListStore,
    view: gtk::ColumnView,
    search: gtk::SearchEntry,
    breadcrumbs: gtk::Box,
    /// Keeps the open folder (or drive) in sync; pressed while it syncs.
    sync_button: gtk::ToggleButton,
    /// Opens the browsed drive's directory in the desktop file manager.
    open_local_button: gtk::Button,
    content: gtk::Stack,
    empty: adw::StatusPage,
    banner: adw::Banner,
    status_icon: gtk::Image,
    status_title: gtk::Label,
    status_detail: gtk::Label,
    /// While a pass runs: how far it is, and what it's doing.
    sync_progress: gtk::Box,
    sync_bar: gtk::ProgressBar,
    sync_doing: gtk::Label,
    /// Storage use under the status: a bar in the brand gradient and "x of y".
    storage: gtk::Box,
    storage_bar: gtk::ProgressBar,
    storage_label: gtk::Label,
}

impl Browser {
    fn new(engine: Rc<Engine>) -> Rc<Self> {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let sorted = gtk::SortListModel::new(Some(store.clone()), None::<gtk::Sorter>);
        let selection = gtk::SingleSelection::new(Some(sorted.clone()));
        let view = gtk::ColumnView::builder()
            .model(&selection)
            .reorderable(false)
            .css_classes(["aerie-files"])
            .build();

        let name = column(
            "Name",
            name_factory(),
            by(|a, b| compare_names(&a.name, &b.name)),
        );
        name.set_expand(true);
        view.append_column(&name);
        let modified = column(
            "Modified",
            text_factory(
                Align::Start,
                |entry| format_modified(entry.modified),
                |entry| Some(format_full_date(entry.modified)),
            ),
            by(|a, b| a.modified.cmp(&b.modified)),
        );
        modified.set_fixed_width(150);
        view.append_column(&modified);
        let size = column(
            "Size",
            text_factory(
                Align::End,
                |entry| entry.size.map_or_else(|| "—".to_owned(), format_size),
                |_| None,
            ),
            by(|a, b| a.size.cmp(&b.size)),
        );
        size.set_fixed_width(96);
        view.append_column(&size);
        // Folders stay first whichever column or direction the user sorts by.
        let order = gtk::MultiSorter::new();
        order.append(gtk::CustomSorter::new(|a, b| {
            entry_of(b).is_folder().cmp(&entry_of(a).is_folder()).into()
        }));
        if let Some(columns) = view.sorter() {
            order.append(columns);
        }
        sorted.set_sorter(Some(&order));
        view.sort_by_column(Some(&name), gtk::SortType::Ascending);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search Drive")
            .hexpand(true)
            .build();
        search.update_property(&[gtk::accessible::Property::KeyShortcuts("Control+K")]);

        let empty = adw::StatusPage::builder().css_classes(["compact"]).build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&view)
            .build();
        let content = gtk::Stack::new();
        content.add_named(&scroller, Some("list"));
        content.add_named(&empty, Some("empty"));

        let transfers_badge = gtk::Label::builder()
            .css_classes(["metadata"])
            .hexpand(true)
            .xalign(1.0)
            .build();
        let transfers_row = sidebar_row("aerie-arrow-down-up-symbolic", "Transfers");
        if let Some(row) = transfers_row.child().and_downcast::<gtk::Box>() {
            row.append(&transfers_badge);
        }
        let browser = Rc::new(Self {
            engine,
            loading_folder: RefCell::default(),
            synced_roots: RefCell::default(),
            active_root: RefCell::default(),
            crumb_mark: Cell::new(SyncMark::None),
            tree: RefCell::default(),
            drives_header: gtk::Button::builder()
                .css_classes(["flat", "drives-toggle"])
                .visible(false)
                .build(),
            drives_count: gtk::Label::builder()
                .css_classes(["drives-count", "numeric"])
                .build(),
            drives_revealer: gtk::Revealer::builder()
                .transition_type(gtk::RevealerTransitionType::SlideDown)
                .transition_duration(180)
                .build(),
            drives_filter: gtk::SearchEntry::builder()
                .placeholder_text("Filter drives")
                .margin_start(12)
                .margin_end(12)
                .margin_bottom(4)
                .visible(false)
                .build(),
            drives_loading: {
                let loading = gtk::Box::builder()
                    .spacing(12)
                    .margin_start(12)
                    .margin_end(12)
                    .margin_top(6)
                    .margin_bottom(6)
                    .css_classes(["dim-label"])
                    .accessible_role(gtk::AccessibleRole::Status)
                    .visible(false)
                    .build();
                loading.append(&adw::Spinner::new());
                loading.append(&gtk::Label::new(Some("Loading shared drives…")));
                loading
            },
            drives_list: gtk::ListBox::builder()
                .css_classes(["navigation-sidebar"])
                .build(),
            drives: RefCell::default(),
            my_drive_badge: sidebar_badge(),
            drive_badges: RefCell::default(),
            synced_drives: RefCell::default(),
            synced_only: gtk::ToggleButton::builder().label("Synced").build(),
            no_synced_drives: gtk::Label::builder()
                .label("No shared drives sync yet.")
                .xalign(0.0)
                .margin_start(18)
                .margin_top(6)
                .css_classes(["metadata"])
                .visible(false)
                .build(),
            places: gtk::ListBox::builder()
                .css_classes(["navigation-sidebar"])
                .build(),
            pages: gtk::Stack::builder()
                .transition_type(gtk::StackTransitionType::Crossfade)
                .transition_duration(120)
                .build(),
            transfers_row,
            transfers_badge,
            sync_row: sidebar_row("aerie-refresh-cw-symbolic", "Sync"),
            sync_state: RefCell::default(),
            selection: selection.clone(),
            state: RefCell::new(samples()),
            account: RefCell::default(),
            indexing: Cell::new(false),
            next_local_id: Cell::new(1),
            store,
            view,
            search,
            sync_button: gtk::ToggleButton::builder()
                .icon_name("aerie-folder-sync-symbolic")
                .visible(false)
                .build(),
            open_local_button: gtk::Button::builder()
                .icon_name("aerie-folder-open-symbolic")
                .tooltip_text("Open drive’s local folder")
                .visible(false)
                .build(),
            breadcrumbs: gtk::Box::builder()
                .spacing(2)
                .css_classes(["breadcrumbs"])
                .accessible_role(gtk::AccessibleRole::Navigation)
                .build(),
            content,
            empty,
            banner: adw::Banner::new(""),
            status_icon: gtk::Image::builder()
                .valign(gtk::Align::Center)
                .css_classes(["status-badge"])
                .build(),
            status_title: gtk::Label::builder()
                .xalign(0.0)
                .css_classes(["status-title"])
                .build(),
            sync_progress: gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .visible(false)
                .build(),
            sync_bar: gtk::ProgressBar::builder()
                .css_classes(["sync-bar"])
                .build(),
            sync_doing: gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .css_classes(["metadata"])
                .build(),
            storage: gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .css_classes(["storage"])
                .visible(false)
                .build(),
            storage_bar: gtk::ProgressBar::builder()
                .css_classes(["storage-bar"])
                .build(),
            storage_label: gtk::Label::builder()
                .xalign(0.0)
                .css_classes(["metadata", "numeric"])
                .build(),
            status_detail: gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["metadata"])
                .build(),
        });

        browser.view.connect_activate(glib::clone!(
            #[weak]
            browser,
            move |view, position| {
                let Some(item) = view.model().and_then(|model| model.item(position)) else {
                    return;
                };
                let entry = entry_of(&item).clone();
                browser.activate(&entry);
            }
        ));
        browser.search.connect_search_changed(glib::clone!(
            #[weak]
            browser,
            move |search| {
                browser.state.borrow_mut().set_query(&search.text());
                browser.refresh();
                // Unindexed drives are searched through Google when online.
                let query = search.text().trim().to_owned();
                if !query.is_empty() && !browser.index_choice().all {
                    browser.engine.send(Command::SearchRemote(query));
                }
            }
        ));
        browser
            .search
            .connect_stop_search(|search| search.set_text(""));
        browser.banner.connect_button_clicked(glib::clone!(
            #[weak]
            browser,
            move |banner| {
                if let Some(window) = banner.root().and_downcast::<gtk::Window>() {
                    let _ = WidgetExt::activate_action(&window, "win.sign-in", None);
                }
                browser.banner.set_revealed(false);
            }
        ));

        browser.selection.connect_selected_item_notify(glib::clone!(
            #[weak]
            browser,
            move |_| browser.update_actions()
        ));
        browser.install_context_menu();

        browser.set_status(Status::Samples);
        browser.refresh();
        browser
    }

    fn selected(&self) -> Option<FileEntry> {
        self.selection
            .selected_item()
            .map(|item| entry_of(&item).clone())
    }

    fn window(&self) -> Option<adw::ApplicationWindow> {
        self.view.root().and_downcast::<adw::ApplicationWindow>()
    }

    /// Enables only the actions that make sense for the selection and the item's
    /// permissions. Sample data supports nothing but new folders.
    /// The folder being browsed as a syncable entry: My Drive or a shared drive at their
    /// top, otherwise the folder itself.
    fn open_folder(&self) -> Option<FileEntry> {
        let state = self.state.borrow();
        let AppCatalog::Drive(catalog) = state.catalog() else {
            return None;
        };
        if state.is_searching() {
            return None;
        }
        let current = state.current_folder();
        if current == state.root() {
            return match self.tree.borrow().as_ref() {
                Some(drive) => Some(drive.clone()),
                None => catalog.my_drive(),
            };
        }
        state.catalog().get(current).filter(FileEntry::is_folder)
    }

    /// Shows whether the open folder syncs, on its own or as part of a synced folder.
    fn update_sync_button(&self) {
        let Some(folder) = self.open_folder() else {
            self.sync_button.set_visible(false);
            return;
        };
        let roots = self.sync_view().1;
        let own = roots.contains(&folder.id);
        let outer = roots
            .iter()
            .find(|root| self.is_inside(&folder.id, root))
            .cloned();
        self.sync_button.set_visible(true);
        self.sync_button.set_active(own || outer.is_some());
        self.sync_button.set_sensitive(outer.is_none());
        let tooltip = match (&outer, own) {
            (Some(outer), _) => format!("Syncs as part of “{}”", self.folder_name(outer)),
            (None, true) => format!("Stop syncing “{}”", folder.name),
            (None, false) => format!("Keep “{}” in sync", folder.name),
        };
        self.sync_button.set_tooltip_text(Some(&tooltip));
    }

    fn update_actions(&self) {
        self.update_sync_button();
        self.open_local_button.set_visible(
            self.current_drive_id()
                .is_some_and(|id| self.local_drive_folder(&id).is_some()),
        );
        let Some(window) = self.window() else { return };
        let drive = self.is_drive();
        let selected = self.selected().filter(|_| drive);
        let can = selected.as_ref().map(|entry| entry.can).unwrap_or_default();
        set_action_enabled(&window, "rename", can.rename);
        set_action_enabled(&window, "move", can.move_item);
        set_action_enabled(&window, "trash", can.trash);
        set_action_enabled(&window, "open-in-drive", selected.is_some());
        let folder = selected.as_ref().filter(|entry| entry.is_folder());
        set_action_enabled(&window, "sync-folder", folder.is_some());
        if let Some(action) = window
            .lookup_action("sync-folder")
            .and_downcast::<gio::SimpleAction>()
        {
            let synced = folder.is_some_and(|f| self.synced_roots.borrow().contains(&f.id));
            action.set_state(&synced.to_variant());
        }
        set_action_enabled(
            &window,
            "download",
            selected
                .as_ref()
                .is_some_and(|entry| !entry.is_folder() && entry.can.download),
        );
        let state = self.state.borrow();
        let folder_allows = state
            .catalog()
            .get(state.current_folder())
            .is_none_or(|folder| folder.can.add_children);
        set_action_enabled(
            &window,
            "new-folder",
            !state.is_searching() && folder_allows,
        );
        set_action_enabled(
            &window,
            "upload",
            drive && !state.is_searching() && folder_allows,
        );
    }

    /// Right-click, or the Menu key, opens actions for the item under the pointer.
    fn install_context_menu(self: &Rc<Self>) {
        let menu = gio::Menu::new();
        menu.append(Some("Open in Google Drive"), Some("win.open-in-drive"));
        menu.append(Some("Download…"), Some("win.download"));
        let sync = gio::Menu::new();
        sync.append(Some("Keep in Sync"), Some("win.sync-folder"));
        menu.append_section(None, &sync);
        let edit = gio::Menu::new();
        edit.append(Some("Rename…"), Some("win.rename"));
        edit.append(Some("Move to…"), Some("win.move"));
        menu.append_section(None, &edit);
        let danger = gio::Menu::new();
        danger.append(Some("Move to Trash"), Some("win.trash"));
        menu.append_section(None, &danger);
        let popover = gtk::PopoverMenu::builder()
            .menu_model(&menu)
            .has_arrow(false)
            .halign(gtk::Align::Start)
            .build();
        popover.set_parent(&self.view);

        let click = gtk::GestureClick::builder()
            .button(gtk::gdk::BUTTON_SECONDARY)
            .build();
        click.connect_pressed(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            #[weak]
            popover,
            move |gesture, _, x, y| {
                if !browser.is_drive() {
                    return;
                }
                browser.select_at(x, y);
                gesture.set_state(gtk::EventSequenceState::Claimed);
                popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                popover.popup();
            }
        ));
        self.view.add_controller(click);
    }

    /// Selects the row under a point, found through the entry ID each cell carries.
    fn select_at(&self, x: f64, y: f64) {
        let mut widget = self.view.pick(x, y, gtk::PickFlags::DEFAULT);
        while let Some(current) = widget {
            if let Some(id) = current.widget_name().strip_prefix(ENTRY_PREFIX) {
                let position = (0..self.selection.n_items()).find(|&position| {
                    self.selection
                        .item(position)
                        .is_some_and(|item| entry_of(&item).id.as_str() == id)
                });
                if let Some(position) = position {
                    self.selection.set_selected(position);
                }
                return;
            }
            widget = current.parent();
        }
    }

    fn rename_selected(self: &Rc<Self>) {
        let Some(entry) = self.selected() else { return };
        let engine = self.engine.clone();
        let id = entry.id.clone();
        crate::actions::rename(&self.view, &entry, move |name| {
            engine.send(Command::Rename {
                id: id.clone(),
                name,
            });
        });
    }

    fn move_selected(self: &Rc<Self>) {
        let Some(entry) = self.selected() else { return };
        let Some(from) = entry.parent.clone() else {
            return;
        };
        let root = self.state.borrow().root().clone();
        let children: crate::actions::Children = Rc::new(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            #[upgrade_or_default]
            move |folder: &DriveFileId| browser.state.borrow().catalog().children(folder)
        ));
        let engine = self.engine.clone();
        let id = entry.id.clone();
        crate::actions::move_to(
            &self.view,
            children,
            root,
            self.root_label(),
            entry,
            move |to| {
                engine.send(Command::Move {
                    id: id.clone(),
                    from: from.clone(),
                    to,
                });
            },
        );
    }

    fn transfers(&self) -> Vec<aerie_store::Transfer> {
        match self.state.borrow().catalog() {
            AppCatalog::Drive(catalog) => catalog.transfers(),
            AppCatalog::Samples(_) => Vec::new(),
        }
    }

    fn toggle_sync_selected(self: &Rc<Self>) {
        if let Some(folder) = self.selected().filter(FileEntry::is_folder) {
            self.toggle_sync_root(folder);
        }
    }

    /// Adds a folder (or all of My Drive, or a shared drive) to the synced folders, or
    /// takes it out. With sync not set up yet, it starts in ~/Aerie when that's free;
    /// otherwise the Sync page asks where.
    fn toggle_sync_root(self: &Rc<Self>, folder: FileEntry) {
        let (settings, mut roots, _) = self.sync_view();
        let name = &folder.name;
        if settings.is_some() {
            let message = if let Some(at) = roots.iter().position(|id| *id == folder.id) {
                roots.remove(at);
                format!("Stopped syncing “{name}”. Its files stay on this computer.")
            } else if let Some(outer) = roots.iter().find(|root| self.is_inside(&folder.id, root)) {
                let outer = self.folder_name(outer);
                return self.toast(adw::Toast::new(&format!(
                    "“{name}” already syncs as part of “{outer}”."
                )));
            } else {
                // Folders inside this one now sync as part of it, not on their own.
                roots.retain(|root| !self.is_inside(root, &folder.id));
                roots.push(folder.id.clone());
                format!("“{name}” will stay in sync.")
            };
            self.engine.send(Command::SetSyncRoots(roots));
            self.toast(adw::Toast::new(&message));
            return;
        }
        let suggested =
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join("Aerie"));
        match suggested.filter(|path| crate::sync_page::is_empty_or_missing(path)) {
            Some(local_root) if std::fs::create_dir_all(&local_root).is_ok() => {
                let shown = local_root.to_string_lossy().into_owned();
                self.engine.send(Command::StartSync {
                    local_root,
                    roots: vec![folder.id.clone()],
                });
                self.toast(adw::Toast::new(&format!(
                    "Syncing “{name}” into {shown}, in the same folders as on Drive"
                )));
            }
            _ => {
                self.show_page("sync");
                self.toast(adw::Toast::new(
                    "Choose a folder on this computer to sync with first.",
                ));
            }
        }
    }

    /// Whether `id` is somewhere inside the folder `outer`.
    fn is_inside(&self, id: &DriveFileId, outer: &DriveFileId) -> bool {
        let state = self.state.borrow();
        let catalog = state.catalog();
        let mut current = catalog.get(id).and_then(|entry| entry.parent);
        // Bounded, so a cycle in bad metadata can't hang the app.
        for _ in 0..256 {
            match current {
                Some(parent) if parent == *outer => return true,
                Some(parent) => current = catalog.get(&parent).and_then(|entry| entry.parent),
                None => return false,
            }
        }
        false
    }

    fn folder_name(&self, id: &DriveFileId) -> String {
        let (_, _, folders) = self.sync_view();
        folders
            .into_iter()
            .find(|folder| folder.id == *id)
            .map_or_else(|| "another synced folder".to_owned(), |folder| folder.name)
    }

    /// Sync settings, chosen roots and top-level folders, for the sync page.
    fn sync_view(
        &self,
    ) -> (
        Option<aerie_store::SyncSettings>,
        Vec<DriveFileId>,
        Vec<FileEntry>,
    ) {
        let state = self.state.borrow();
        match state.catalog() {
            AppCatalog::Drive(catalog) => {
                // All of My Drive first, then its top-level folders and the shared drives.
                let mut folders: Vec<FileEntry> = catalog.my_drive().into_iter().collect();
                folders.extend(
                    catalog
                        .root()
                        .map(|root| catalog.children(&root))
                        .unwrap_or_default(),
                );
                folders.extend(catalog.shared_drives());
                let roots = catalog.sync_roots();
                // Folders synced from deeper in the tree, through their menu.
                for root in &roots {
                    if !folders.iter().any(|folder| folder.id == *root)
                        && let Some(entry) = catalog.get(root)
                    {
                        folders.push(entry);
                    }
                }
                (catalog.sync_settings(), roots, folders)
            }
            AppCatalog::Samples(_) => (None, Vec::new(), Vec::new()),
        }
    }

    fn set_sync_state(&self, state: crate::engine::SyncState) {
        let off = state == crate::engine::SyncState::Off;
        self.sync_state.replace((!off).then_some(state));
        self.set_status(Status::UpToDate);
        self.update_actions();
    }

    fn set_transfer_count(&self, active: usize) {
        self.transfers_badge.set_label(&if active == 0 {
            String::new()
        } else {
            active.to_string()
        });
    }

    fn upload(self: &Rc<Self>, files: Vec<gio::File>) {
        let parent = self.state.borrow().current_folder().clone();
        let files: Vec<LocalFile> = files
            .into_iter()
            .filter_map(|file| file.path())
            .map(|path| {
                let (content_type, _) = gio::content_type_guess(Some(&path), None);
                let mime_type = gio::content_type_get_mime_type(&content_type)
                    .map_or_else(|| "application/octet-stream".to_owned(), String::from);
                LocalFile { path, mime_type }
            })
            .collect();
        if files.is_empty() {
            return;
        }
        let what = match files.as_slice() {
            [file] => format!(
                "“{}”",
                file.path.file_name().unwrap_or_default().to_string_lossy()
            ),
            files => format!("{} files", files.len()),
        };
        let folder = {
            let state = self.state.borrow();
            if &parent == state.root() {
                None
            } else {
                state.catalog().get(&parent).map(|folder| folder.name)
            }
        }
        .unwrap_or_else(|| self.root_label());
        self.engine.send(Command::Upload { files, parent });

        let toast = adw::Toast::builder()
            .title(format!("Uploading {what} to “{folder}”"))
            .button_label("Show")
            .build();
        toast.connect_button_clicked(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            move |_| browser.show_transfers()
        ));
        self.toast(toast);
    }

    fn show_transfers(&self) {
        self.show_page("transfers");
    }

    /// Shows the Transfers or Sync page, selecting its row in the sidebar.
    /// Shows the Transfers, Sync or Activity page, selecting its row in the sidebar.
    fn show_page(&self, page: &str) {
        self.drives_list.unselect_all();
        match page {
            "sync" | "activity" => {
                self.sync_row.set_visible(true);
                self.places.select_row(Some(&self.sync_row));
            }
            _ => {
                self.transfers_row.set_visible(true);
                self.places.select_row(Some(&self.transfers_row));
            }
        }
        self.pages.set_visible_child_name(page);
    }

    fn toast(&self, toast: adw::Toast) {
        if let Some(overlay) = self
            .window()
            .and_then(|window| window.content())
            .and_downcast::<adw::ToastOverlay>()
        {
            overlay.add_toast(toast);
        }
    }

    fn choose_uploads(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Upload to Google Drive")
            .accept_label("Upload")
            .modal(true)
            .build();
        dialog.open_multiple(
            self.window().as_ref(),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = browser)]
                self,
                move |result| {
                    let Ok(files) = result else { return };
                    let files = (0..files.n_items())
                        .filter_map(|index| files.item(index).and_downcast::<gio::File>())
                        .collect();
                    browser.upload(files);
                }
            ),
        );
    }

    /// Files dropped on the list upload into the open folder.
    fn install_drop_target(self: &Rc<Self>, target: &impl IsA<gtk::Widget>) {
        use gtk::gdk::DragAction;
        // File managers offer copy, move or link and leave the choice to the compositor;
        // accept them all, ask for a copy, and never remove anything from the source.
        let drop = gtk::DropTarget::new(
            glib::Type::INVALID,
            DragAction::COPY | DragAction::MOVE | DragAction::LINK,
        );
        drop.set_types(&[gtk::gdk::FileList::static_type(), gio::File::static_type()]);
        drop.connect_enter(|target, _, _| {
            if let Some(offer) = target.current_drop() {
                tracing::info!(formats = %offer.formats(), actions = ?offer.actions(), "files dragged over the list");
            }
            DragAction::COPY
        });
        drop.connect_motion(|_, _, _| DragAction::COPY);
        drop.connect_drop(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, _| {
                let files = if let Ok(list) = value.get::<gtk::gdk::FileList>() {
                    list.files()
                } else if let Ok(file) = value.get::<gio::File>() {
                    vec![file]
                } else {
                    tracing::warn!(kind = %value.type_(), "a drop carried no files");
                    return false;
                };
                tracing::info!(files = files.len(), "files dropped on the list");
                let enabled = browser
                    .window()
                    .and_then(|window| window.lookup_action("upload"))
                    .is_some_and(|action| action.is_enabled());
                if !enabled {
                    browser.toast(adw::Toast::new(if browser.state.borrow().is_searching() {
                        "Clear the search to upload into a folder."
                    } else {
                        "You can’t add files to this folder."
                    }));
                    return false;
                }
                browser.upload(files);
                true
            }
        ));
        target.add_controller(drop);
    }

    fn download_selected(self: &Rc<Self>) {
        let Some(entry) = self.selected() else { return };
        if entry.is_google_native() {
            let formats = crate::actions::export_formats(&entry.mime_type);
            let browser = self.clone();
            crate::actions::choose_export(&self.view, &entry.clone(), formats, move |format| {
                browser.save_download(entry.clone(), Some(format));
            });
        } else {
            self.save_download(entry, None);
        }
    }

    fn save_download(
        self: &Rc<Self>,
        entry: FileEntry,
        export: Option<crate::actions::ExportFormat>,
    ) {
        let name = match &export {
            Some(format) => format!("{}.{}", entry.name, format.extension),
            None => entry.name.clone(),
        };
        let dialog = gtk::FileDialog::builder()
            .title("Download")
            .initial_name(&name)
            .accept_label("Download")
            .modal(true)
            .build();
        if let Some(downloads) = glib::user_special_dir(glib::UserDirectory::Downloads) {
            dialog.set_initial_folder(Some(&gio::File::for_path(downloads)));
        }
        let engine = self.engine.clone();
        dialog.save(
            self.window().as_ref(),
            gio::Cancellable::NONE,
            move |result| {
                let Some(destination) = result.ok().and_then(|file| file.path()) else {
                    return;
                };
                engine.send(Command::Download {
                    entry,
                    destination,
                    export_mime: export.map(|format| format.mime_type.to_owned()),
                });
            },
        );
    }

    fn trash_selected(&self) {
        if let Some(entry) = self.selected() {
            self.engine.send(Command::Trash { id: entry.id });
        }
    }

    fn open_selected_in_drive(&self) {
        if let (Some(entry), Some(window)) = (self.selected(), self.window()) {
            open_uri(&window, &drive_url(&entry));
        }
    }

    fn current_drive_id(&self) -> Option<DriveFileId> {
        let state = self.state.borrow();
        let AppCatalog::Drive(catalog) = state.catalog() else {
            return None;
        };
        self.tree
            .borrow()
            .as_ref()
            .map(|drive| drive.id.clone())
            .or_else(|| catalog.root())
    }

    fn local_drive_folder(&self, id: &DriveFileId) -> Option<std::path::PathBuf> {
        match self.state.borrow().catalog() {
            AppCatalog::Drive(catalog) => catalog.local_drive_folder(id),
            AppCatalog::Samples(_) => None,
        }
    }

    fn open_local_drive(self: &Rc<Self>, id: &DriveFileId) {
        let Some(path) = self.local_drive_folder(id) else {
            self.toast(adw::Toast::new("This drive has no local sync folder."));
            return;
        };
        if !path.is_dir() {
            self.toast(adw::Toast::new(
                "This drive’s local folder isn’t available yet.",
            ));
            return;
        }
        let file = gio::File::for_path(path);
        gtk::FileLauncher::new(Some(&file)).launch(
            self.window().as_ref(),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = browser)]
                self,
                move |result| {
                    if let Err(err) = result
                        && !err.matches(gtk::DialogError::Dismissed)
                        && !err.matches(gtk::DialogError::Cancelled)
                    {
                        browser.toast(adw::Toast::new(&format!(
                            "Couldn’t open the local folder: {err}"
                        )));
                    }
                }
            ),
        );
    }

    fn split_view(
        self: &Rc<Self>,
        transfers: &gtk::Widget,
        sync: &gtk::Widget,
        activity: &gtk::Widget,
    ) -> adw::NavigationSplitView {
        let content = adw::ToolbarView::new();
        let header = adw::HeaderBar::builder()
            .title_widget(
                &adw::Clamp::builder()
                    .maximum_size(480)
                    .child(&self.search)
                    .build(),
            )
            .build();
        let new_folder = gtk::Button::builder()
            .icon_name("aerie-folder-plus-symbolic")
            .tooltip_text("New folder (Ctrl+Shift+N)")
            .action_name("win.new-folder")
            .build();
        // Hidden rather than greyed out while unavailable, as the plan asks.
        new_folder
            .bind_property("sensitive", &new_folder, "visible")
            .sync_create()
            .build();
        header.pack_end(&new_folder);
        content.add_top_bar(&header);
        content.add_top_bar(&self.banner);

        let upload = gtk::Button::builder()
            .icon_name("aerie-upload-symbolic")
            .tooltip_text("Upload files (Ctrl+U)")
            .action_name("win.upload")
            .build();
        upload
            .bind_property("sensitive", &upload, "visible")
            .sync_create()
            .build();
        header.pack_end(&upload);
        // The button only reflects state; a click asks for the change, and the next
        // refresh shows the result.
        self.sync_button.connect_clicked(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            move |_| {
                if let Some(folder) = browser.open_folder() {
                    browser.toggle_sync_root(folder);
                }
                browser.update_sync_button();
            }
        ));
        header.pack_end(&self.sync_button);
        self.open_local_button.connect_clicked(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            move |_| {
                if let Some(id) = browser.current_drive_id() {
                    browser.open_local_drive(&id);
                }
            }
        ));
        header.pack_end(&self.open_local_button);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&self.breadcrumbs);
        body.append(&self.content);
        content.set_content(Some(&body));
        body.add_css_class("drop-zone");
        self.install_drop_target(&body);

        self.pages.add_named(&content, Some("files"));
        self.pages.add_named(transfers, Some("transfers"));
        self.pages.add_named(sync, Some("sync"));
        self.pages.add_named(activity, Some("activity"));

        let sidebar = gtk::Overlay::builder().child(&self.sidebar()).build();
        let split = adw::NavigationSplitView::builder()
            .sidebar(&adw::NavigationPage::new(&sidebar, "Aerie"))
            .content(&adw::NavigationPage::new(&self.pages, "My Drive"))
            .min_sidebar_width(200.0)
            .max_sidebar_width(260.0)
            .build();
        if let Some(width) = crate::preferences::Settings::load().sidebar_width {
            set_sidebar_width(&split, width);
        }
        sidebar.add_overlay(&resize_handle(&split, &sidebar));
        split
    }

    fn sidebar(self: &Rc<Self>) -> gtk::Widget {
        let places = &self.places;
        let my_drive = sidebar_row("aerie-folder-symbolic", "My Drive");
        if let Some(row) = my_drive.child().and_downcast::<gtk::Box>() {
            // Pushed to the row's end, as in the shared drive rows.
            self.my_drive_badge.set_hexpand(true);
            row.append(&self.my_drive_badge);
        }
        places.append(&my_drive);
        places.append(&self.transfers_row);
        places.append(&self.sync_row);
        self.transfers_row.set_visible(false);
        self.sync_row.set_visible(false);
        places.select_row(places.row_at_index(0).as_ref());
        places.connect_row_activated(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            move |_, row| {
                browser.drives_list.unselect_all();
                if row.index() == 1 {
                    browser.pages.set_visible_child_name("transfers");
                } else if row.index() == 2 {
                    browser.pages.set_visible_child_name("sync");
                } else {
                    browser.pages.set_visible_child_name("files");
                    browser.open_tree(None);
                }
            }
        ));

        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.append(&self.status_title);
        text.append(&self.status_detail);
        let summary = gtk::Box::builder().spacing(12).build();
        summary.append(&self.status_icon);
        summary.append(&text);
        // While paused, the pause badge itself resumes sync.
        let resume = gtk::GestureClick::new();
        resume.connect_pressed(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            move |gesture, _, _, _| {
                if browser.status_icon.has_css_class("paused") {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    browser.engine.send(Command::ResumeSync);
                }
            }
        ));
        self.status_icon.add_controller(resume);
        self.storage.append(&self.storage_bar);
        self.storage.append(&self.storage_label);
        let status = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .css_classes(["sync-status", "card", "activatable"])
            .accessible_role(gtk::AccessibleRole::Status)
            .tooltip_text("Show sync activity")
            .cursor(&gtk::gdk::Cursor::from_name("pointer", None).expect("a standard cursor"))
            .build();
        // The status card opens the activity log.
        let open = gtk::GestureClick::new();
        open.connect_released(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            move |_, _, _, _| {
                if browser.is_drive() {
                    browser.show_page("activity");
                }
            }
        ));
        status.add_controller(open);
        self.sync_progress.append(&self.sync_bar);
        self.sync_progress.append(&self.sync_doing);
        status.append(&summary);
        status.append(&self.sync_progress);
        status.append(&self.storage);

        self.drives_list.connect_row_activated(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            move |_, row| {
                let drive = browser.drives.borrow().get(row.index() as usize).cloned();
                if let Some(drive) = drive {
                    browser.places.unselect_all();
                    browser.pages.set_visible_child_name("files");
                    browser.open_tree(Some(drive));
                }
            }
        ));
        self.install_drive_section();
        self.install_sidebar_menu();
        let lists = gtk::Box::new(gtk::Orientation::Vertical, 6);
        lists.append(places);
        lists.append(&self.drives_header);
        lists.append(&self.drives_loading);
        lists.append(&self.drives_revealer);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&lists)
            .build();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&scroller);
        body.append(&status);

        let title = gtk::Box::builder().spacing(8).build();
        title.append(
            &gtk::Image::builder()
                .icon_name(concat!("ai.lastrefuge.Aerie", "-mark"))
                .pixel_size(24)
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
        title.append(
            &gtk::Label::builder()
                .label("Aerie")
                .css_classes(["brand-title"])
                .build(),
        );
        let view = adw::ToolbarView::new();
        let header = adw::HeaderBar::builder().title_widget(&title).build();
        header.pack_end(&crate::app_menu::button());
        view.add_top_bar(&header);
        view.set_content(Some(&body));
        view.upcast()
    }

    fn is_drive(&self) -> bool {
        self.state.borrow().catalog().is_drive()
    }

    fn show_drive(self: &Rc<Self>, account: &Account) {
        let catalog = match StoreCatalog::open(account.id.clone()) {
            Ok(catalog) => catalog,
            Err(err) => {
                tracing::error!(%err, "couldn't open the metadata cache");
                return;
            }
        };
        // Before the first inventory finishes, the root isn't known; the alias works for
        // nothing, so show the indexing state until it arrives.
        let root = catalog.root().unwrap_or_else(|| DriveFileId::new("root"));
        self.indexing.set(!catalog.inventory_complete());
        self.state
            .borrow_mut()
            .replace(root, AppCatalog::Drive(catalog));
        self.account.replace(Some(account.clone()));
        self.tree.replace(None);
        self.transfers_row.set_visible(true);
        self.sync_row.set_visible(true);
        self.banner.set_revealed(false);
        self.set_status(if self.indexing.get() {
            Status::Indexing(0)
        } else {
            Status::UpToDate
        });
        // The cached drives show at once; the engine reports changes only when there are some.
        self.refresh_drives();
        self.show_state();
    }

    /// Shows how far a running sync pass is, in the status card; `None` hides it.
    fn set_sync_progress(&self, progress: Option<&crate::sync_page::Progress>) {
        let Some(progress) = progress else {
            self.sync_progress.set_visible(false);
            return;
        };
        self.sync_bar.set_fraction(progress.fraction());
        self.sync_doing.set_label(&progress.doing());
        self.sync_doing.set_tooltip_text(Some(&progress.doing()));
        self.status_detail.set_label(&progress.count());
        self.sync_progress.set_visible(true);
    }

    /// A synced folder's name, including My Drive and shared drives.
    fn folder_title(&self, id: &DriveFileId) -> Option<String> {
        let state = self.state.borrow();
        match state.catalog() {
            AppCatalog::Drive(catalog) => catalog
                .get(id)
                .or_else(|| catalog.my_drive().filter(|entry| entry.id == *id))
                .map(|entry| entry.name),
            AppCatalog::Samples(_) => None,
        }
    }

    /// Shows storage use; `None` hides it.
    fn set_storage(&self, storage: Option<(u64, Option<u64>)>) {
        let Some((used, limit)) = storage else {
            self.storage.set_visible(false);
            return;
        };
        match limit {
            Some(limit) if limit > 0 => {
                let fraction = (used as f64 / limit as f64).clamp(0.0, 1.0);
                self.storage_bar.set_fraction(fraction);
                self.storage_bar.set_visible(true);
                self.storage_label.set_label(&format!(
                    "{} of {} used",
                    format_size(used),
                    format_size(limit)
                ));
                if fraction >= 0.9 {
                    self.storage_bar.add_css_class("nearly-full");
                } else {
                    self.storage_bar.remove_css_class("nearly-full");
                }
            }
            _ => {
                self.storage_bar.set_visible(false);
                self.storage_label
                    .set_label(&format!("{} used", format_size(used)));
            }
        }
        self.storage.set_visible(true);
    }

    fn show_samples(self: &Rc<Self>) {
        self.set_storage(None);
        self.state.replace(samples());
        self.account.replace(None);
        self.tree.replace(None);
        self.transfers_row.set_visible(false);
        self.sync_row.set_visible(false);
        self.sync_state.replace(None);
        self.pages.set_visible_child_name("files");
        self.indexing.set(false);
        self.set_status(Status::Samples);
        self.refresh_drives();
        self.show_state();
    }

    /// Re-reads the catalog after the engine stored new metadata.
    fn reload(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            if let AppCatalog::Drive(catalog) = state.catalog()
                && let Some(root) = catalog.root()
                && &root != state.root()
                && self.tree.borrow().is_none()
            {
                // The first inventory just reported the real root ID.
                let AppCatalog::Drive(catalog) = std::mem::replace(
                    state.catalog_mut(),
                    AppCatalog::Samples(MemoryCatalog::default()),
                ) else {
                    unreachable!()
                };
                state.replace(root, AppCatalog::Drive(catalog));
            }
            if let AppCatalog::Drive(catalog) = state.catalog() {
                self.indexing.set(!catalog.inventory_complete());
            }
            state.revalidate();
        }
        self.refresh_drives();
        self.refresh();
    }

    /// Lists the shared drives in the sidebar, keeping the open one if it still exists.
    fn refresh_drives(self: &Rc<Self>) {
        let (drives, signed_in) = match self.state.borrow().catalog() {
            AppCatalog::Drive(catalog) => (catalog.shared_drives(), true),
            AppCatalog::Samples(_) => (Vec::new(), false),
        };
        // The first index lists shared drives before any files; until then, say so.
        let loading = signed_in && drives.is_empty() && self.indexing.get();
        self.drives_loading.set_visible(loading);
        self.drives_header
            .set_visible(loading || !drives.is_empty());
        self.drives_count.set_label(&if drives.is_empty() {
            String::new()
        } else {
            drives.len().to_string()
        });
        self.drives_filter
            .set_visible(drives.len() > FILTER_DRIVES_ABOVE);
        self.drives_revealer.set_visible(!drives.is_empty());
        // Pinned drives come first, each group in name order.
        let pinned = crate::preferences::Settings::load().pinned_drives;
        let is_pinned = |drive: &FileEntry| pinned.iter().any(|id| id == drive.id.as_str());
        let mut drives = drives;
        drives.sort_by_key(|drive| !is_pinned(drive));
        if *self.drives.borrow() == drives {
            return;
        }
        while let Some(row) = self.drives_list.first_child() {
            self.drives_list.remove(&row);
        }
        self.drive_badges.borrow_mut().clear();
        for drive in &drives {
            self.drives_list
                .append(&self.drive_row(drive, is_pinned(drive)));
        }
        let open = self.tree.borrow().as_ref().map(|tree| tree.id.clone());
        if let Some(open) = open {
            match drives.iter().position(|drive| drive.id == open) {
                Some(index) => self
                    .drives_list
                    .select_row(self.drives_list.row_at_index(index as i32).as_ref()),
                // The drive went away; return to My Drive.
                None => self.open_tree(None),
            }
        }
        *self.drives.borrow_mut() = drives;
    }

    /// Right-clicking a drive offers sync controls and its local folder.
    fn install_sidebar_menu(self: &Rc<Self>) {
        for list in [&self.places, &self.drives_list] {
            let click = gtk::GestureClick::builder()
                .button(gtk::gdk::BUTTON_SECONDARY)
                .build();
            let is_drives = list == &self.drives_list;
            click.connect_pressed(glib::clone!(
                #[weak(rename_to = browser)]
                self,
                #[weak]
                list,
                move |gesture, _, x, y| {
                    let Some(row) = list.row_at_y(y as i32) else {
                        return;
                    };
                    let target = if is_drives {
                        browser.drives.borrow().get(row.index() as usize).cloned()
                    } else if row.index() == 0 {
                        match browser.state.borrow().catalog() {
                            AppCatalog::Drive(catalog) => catalog.my_drive(),
                            AppCatalog::Samples(_) => None,
                        }
                    } else {
                        None
                    };
                    let Some(target) = target else { return };
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    browser.show_sync_menu(&list, target, x, y);
                }
            ));
            list.add_controller(click);
        }
    }

    fn show_sync_menu(self: &Rc<Self>, parent: &gtk::ListBox, target: FileEntry, x: f64, y: f64) {
        let synced = self.sync_view().1.contains(&target.id);
        let menu = gio::Menu::new();
        if self.local_drive_folder(&target.id).is_some() {
            let open = gio::MenuItem::new(Some("Open local folder"), None);
            open.set_action_and_target_value(
                Some("win.open-local-folder"),
                Some(&target.id.as_str().to_variant()),
            );
            menu.append_item(&open);
        }
        let item = gio::MenuItem::new(
            Some(if synced {
                "Stop Syncing"
            } else {
                "Keep in Sync"
            }),
            None,
        );
        item.set_action_and_target_value(
            Some("win.sync-root"),
            Some(&target.id.as_str().to_variant()),
        );
        menu.append_item(&item);
        let popover = gtk::PopoverMenu::builder()
            .menu_model(&menu)
            .has_arrow(false)
            .halign(gtk::Align::Start)
            .build();
        popover.set_parent(parent);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            // Unparent after the menu's action has run.
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
    }

    /// The sidebar menu's target: My Drive or a shared drive, by ID.
    fn toggle_sync_root_by_id(self: &Rc<Self>, id: &str) {
        let target = match self.state.borrow().catalog() {
            AppCatalog::Drive(catalog) => catalog
                .my_drive()
                .filter(|entry| entry.id.as_str() == id)
                .or_else(|| {
                    self.drives
                        .borrow()
                        .iter()
                        .find(|drive| drive.id.as_str() == id)
                        .cloned()
                }),
            AppCatalog::Samples(_) => None,
        };
        if let Some(target) = target {
            self.toggle_sync_root(target);
        }
    }

    /// Wires the shared drives heading (fold and unfold), the filter and the list's order.
    fn install_drive_section(self: &Rc<Self>) {
        let heading = gtk::Box::builder().spacing(8).build();
        heading.append(
            &gtk::Label::builder()
                .label("Shared drives")
                .xalign(0.0)
                .hexpand(true)
                .css_classes(["sidebar-heading"])
                .build(),
        );
        heading.append(&self.drives_count);
        heading.append(
            &gtk::Image::builder()
                .icon_name("aerie-chevron-right-symbolic")
                .pixel_size(14)
                .css_classes(["chevron"])
                .build(),
        );
        self.drives_header.set_child(Some(&heading));

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        // All or synced drives, as a small two-way switch.
        let all = gtk::ToggleButton::builder().label("All").build();
        self.synced_only.set_group(Some(&all));
        let choice = gtk::Box::builder()
            .css_classes(["linked", "drive-scope"])
            .halign(gtk::Align::Start)
            .margin_start(12)
            .margin_bottom(6)
            .build();
        choice.append(&all);
        choice.append(&self.synced_only);
        let synced_only = crate::preferences::Settings::load().synced_drives_only;
        if synced_only {
            self.synced_only.set_active(true);
        } else {
            all.set_active(true);
        }
        self.synced_only.connect_toggled(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            move |button| {
                let mut settings = crate::preferences::Settings::load();
                settings.synced_drives_only = button.is_active();
                settings.save();
                browser.drives_list.invalidate_filter();
                browser.update_no_synced_drives();
            }
        ));
        content.append(&choice);
        content.append(&self.drives_filter);
        content.append(&self.drives_list);
        content.append(&self.no_synced_drives);
        self.drives_revealer.set_child(Some(&content));

        let expanded = !crate::preferences::Settings::load().drives_collapsed;
        self.set_drives_expanded(expanded);
        self.drives_header.connect_clicked(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            move |_| {
                let expanded = !browser.drives_revealer.reveals_child();
                browser.set_drives_expanded(expanded);
                let mut settings = crate::preferences::Settings::load();
                settings.drives_collapsed = !expanded;
                settings.save();
            }
        ));

        self.drives_list.set_filter_func(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            #[upgrade_or]
            true,
            move |row| {
                let drives = browser.drives.borrow();
                let Some(drive) = drives.get(row.index() as usize) else {
                    return true;
                };
                let query = browser.drives_filter.text().to_lowercase();
                let named = query.is_empty() || drive.name.to_lowercase().contains(&query);
                let shown = !browser.synced_only.is_active()
                    || browser.synced_drives.borrow().contains(&drive.id);
                named && shown
            }
        ));
        self.drives_filter.connect_search_changed(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            move |_| browser.drives_list.invalidate_filter()
        ));
    }

    fn set_drives_expanded(&self, expanded: bool) {
        self.drives_revealer.set_reveal_child(expanded);
        if expanded {
            self.drives_header.add_css_class("expanded");
        } else {
            self.drives_header.remove_css_class("expanded");
        }
        self.drives_header.set_tooltip_text(Some(if expanded {
            "Hide shared drives"
        } else {
            "Show shared drives"
        }));
    }

    /// A shared drive in the sidebar: its icon tinted a color of its own, its name, and a
    /// pin that shows on hover, or always once pinned.
    fn drive_row(self: &Rc<Self>, drive: &FileEntry, pinned: bool) -> gtk::ListBoxRow {
        let row = gtk::Box::builder().spacing(12).build();
        row.append(
            &gtk::Image::builder()
                .icon_name("aerie-hard-drive-symbolic")
                .css_classes(["drive-icon", drive_hue(&drive.id)])
                .build(),
        );
        row.append(
            &gtk::Label::builder()
                .label(&drive.name)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .tooltip_text(&drive.name)
                .build(),
        );
        let pin = gtk::Button::builder()
            .icon_name("aerie-pin-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular", "drive-pin"])
            .tooltip_text(if pinned { "Unpin" } else { "Pin to top" })
            .build();
        if pinned {
            pin.add_css_class("pinned");
        }
        let id = drive.id.as_str().to_owned();
        pin.connect_clicked(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            move |_| {
                let mut settings = crate::preferences::Settings::load();
                if let Some(at) = settings.pinned_drives.iter().position(|p| *p == id) {
                    settings.pinned_drives.remove(at);
                } else {
                    settings.pinned_drives.push(id.clone());
                }
                settings.save();
                browser.refresh_drives();
            }
        ));
        let badge = sidebar_badge();
        row.append(&badge);
        self.drive_badges
            .borrow_mut()
            .push((drive.id.clone(), badge));
        row.append(&pin);
        gtk::ListBoxRow::builder().child(&row).build()
    }

    /// Opens My Drive (`None`) or a shared drive at its top.
    fn open_tree(self: &Rc<Self>, drive: Option<FileEntry>) {
        let root = match &drive {
            Some(drive) => drive.id.clone(),
            None => match self.state.borrow().catalog() {
                AppCatalog::Drive(catalog) => {
                    catalog.root().unwrap_or_else(|| DriveFileId::new("root"))
                }
                AppCatalog::Samples(_) => DriveFileId::new(ROOT_ID),
            },
        };
        self.tree.replace(drive);
        self.state.borrow_mut().switch_root(root);
        self.show_state();
    }

    /// The name at the top of the breadcrumbs.
    fn root_label(&self) -> String {
        self.tree
            .borrow()
            .as_ref()
            .map_or_else(|| "My Drive".to_owned(), |drive| drive.name.clone())
    }

    fn set_status(&self, status: Status) {
        let email = self
            .account
            .borrow()
            .as_ref()
            .map(|account| account.email.clone())
            .unwrap_or_default();
        let (tone, icon, title, detail) = match status {
            Status::Samples => (
                Tone::Off,
                "aerie-cloud-off-symbolic",
                "Not connected".to_owned(),
                "Sample files only".to_owned(),
            ),
            Status::Indexing(0) => (
                Tone::Working,
                "aerie-loader-circle-symbolic",
                "Checking for changes".to_owned(),
                email.clone(),
            ),
            Status::Indexing(files) => (
                Tone::Working,
                "aerie-loader-circle-symbolic",
                "Indexing your Drive".to_owned(),
                format!("{files} items so far"),
            ),
            Status::UpToDate => match self.sync_state.borrow().as_ref() {
                Some(crate::engine::SyncState::InSync) => (
                    Tone::Good,
                    "aerie-cloud-check-symbolic",
                    "Synced".to_owned(),
                    "All files up to date".to_owned(),
                ),
                Some(crate::engine::SyncState::Syncing) => (
                    Tone::Working,
                    "aerie-refresh-cw-symbolic",
                    "Syncing".to_owned(),
                    "Updating your files".to_owned(),
                ),
                Some(crate::engine::SyncState::Paused) => (
                    Tone::Off,
                    "aerie-pause-symbolic",
                    "Paused".to_owned(),
                    "Sync is paused".to_owned(),
                ),
                Some(
                    crate::engine::SyncState::Attention(_)
                    | crate::engine::SyncState::HeldDeletions(_),
                ) => (
                    Tone::Warning,
                    "aerie-triangle-alert-symbolic",
                    "Needs attention".to_owned(),
                    "Open Sync for details".to_owned(),
                ),
                _ => (
                    Tone::Good,
                    "aerie-cloud-check-symbolic",
                    "Up to date".to_owned(),
                    "Your Drive is current".to_owned(),
                ),
            },
            Status::Offline => (
                Tone::Off,
                "aerie-cloud-off-symbolic",
                "Offline".to_owned(),
                "Showing files from the last update".to_owned(),
            ),
            Status::NeedsSignIn => (
                Tone::Warning,
                "aerie-triangle-alert-symbolic",
                "Needs attention".to_owned(),
                "Sign in again to stay up to date".to_owned(),
            ),
        };
        self.status_icon
            .set_css_classes(&["status-badge", tone.class()]);
        let paused = matches!(status, Status::UpToDate)
            && matches!(
                self.sync_state.borrow().as_ref(),
                Some(crate::engine::SyncState::Paused)
            );
        if paused {
            self.status_icon.add_css_class("paused");
            self.status_icon.set_tooltip_text(Some("Resume syncing"));
            self.status_icon
                .set_cursor(gtk::gdk::Cursor::from_name("pointer", None).as_ref());
        } else {
            self.status_icon.set_tooltip_text(None);
            self.status_icon.set_cursor(None);
        }
        // The account shows on hover; the card itself says how things stand.
        let account = (!email.is_empty()).then_some(email.as_str());
        self.status_title.set_tooltip_text(account);
        self.status_detail.set_tooltip_text(account);
        self.status_icon.set_icon_name(Some(icon));
        self.status_title.set_label(&title);
        self.status_detail.set_label(&detail);

        match status {
            Status::Samples => {
                self.banner
                    .set_title("Showing sample files. Google Drive isn’t connected.");
                self.banner.set_button_label(Some("Connect"));
                self.banner.set_revealed(true);
            }
            Status::NeedsSignIn => {
                self.banner.set_title("Your Google sign-in has expired.");
                self.banner.set_button_label(Some("Sign In"));
                self.banner.set_revealed(true);
            }
            Status::Offline => {
                self.banner
                    .set_title("You’re offline. Showing files from the last update.");
                self.banner.set_button_label(None);
                self.banner.set_revealed(true);
            }
            Status::Indexing(_) | Status::UpToDate => self.banner.set_revealed(false),
        }
    }

    fn activate(self: &Rc<Self>, entry: &FileEntry) {
        if entry.is_folder() {
            let opened = self.state.borrow_mut().open(&entry.id);
            if opened {
                self.show_state();
            }
        } else if self.is_drive() {
            // Opening and downloading arrive with transfers; for now, open it in Drive.
            if let Some(window) = self.window() {
                open_uri(&window, &drive_url(entry));
            }
        }
    }

    /// A folder is synced when it, or a folder above it, is a sync root.
    fn mark(&self, catalog: &AppCatalog, folder: &DriveFileId) -> SyncMark {
        let roots = self.synced_roots.borrow();
        if roots.is_empty() || !catalog.is_drive() {
            return SyncMark::None;
        }
        let active = self.active_root.borrow();
        let mut current = Some(folder.clone());
        // Folder trees are shallow; the bound guards against a cycle in bad data.
        for _ in 0..64 {
            let Some(id) = current else { break };
            if roots.contains(&id) {
                return if active.as_ref() == Some(&id) {
                    SyncMark::Syncing
                } else {
                    SyncMark::Synced
                };
            }
            current = catalog.get(&id).and_then(|entry| entry.parent);
        }
        SyncMark::None
    }

    fn set_synced_roots(self: &Rc<Self>, roots: Vec<DriveFileId>) {
        let roots: std::collections::HashSet<_> = roots.into_iter().collect();
        if *self.synced_roots.borrow() != roots {
            self.synced_roots.replace(roots);
            self.refresh();
        }
    }

    /// Marks the synced folder a pass is working in, or none.
    /// Which shared drives have something syncing, for the synced-only filter.
    fn update_synced_drives(&self) {
        let roots = match self.state.borrow().catalog() {
            AppCatalog::Drive(catalog) => catalog.sync_roots(),
            AppCatalog::Samples(_) => Vec::new(),
        };
        let mut synced = std::collections::HashSet::new();
        {
            let state = self.state.borrow();
            let catalog = state.catalog();
            for root in roots {
                let mut current = Some(root);
                // Bounded, so a cycle in bad metadata can't hang the app.
                for _ in 0..256 {
                    let Some(id) = current else { break };
                    match catalog.get(&id) {
                        Some(entry) if entry.mime_type == aerie_drive::api::SHARED_DRIVE_MIME => {
                            synced.insert(entry.id);
                            break;
                        }
                        Some(entry) => current = entry.parent,
                        None => break,
                    }
                }
            }
        }
        if *self.synced_drives.borrow() != synced {
            self.synced_drives.replace(synced);
            self.drives_list.invalidate_filter();
        }
        self.update_no_synced_drives();
    }

    fn update_no_synced_drives(&self) {
        let empty = self.synced_only.is_active()
            && !self.drives.borrow().is_empty()
            && self.synced_drives.borrow().is_empty();
        self.no_synced_drives.set_visible(empty);
    }

    /// Badges My Drive and the shared drives in the sidebar that sync as a whole.
    fn update_sidebar_marks(&self) {
        self.update_synced_drives();
        let roots = self.synced_roots.borrow();
        let active = self.active_root.borrow();
        let mark = |id: &DriveFileId| match (roots.contains(id), active.as_ref() == Some(id)) {
            (false, _) => SyncMark::None,
            (true, true) => SyncMark::Syncing,
            (true, false) => SyncMark::Synced,
        };
        let my_drive = match self.state.borrow().catalog() {
            AppCatalog::Drive(catalog) => catalog.root(),
            AppCatalog::Samples(_) => None,
        };
        show_mark(
            &self.my_drive_badge,
            my_drive.as_ref().map_or(SyncMark::None, mark),
        );
        for (id, badge) in self.drive_badges.borrow().iter() {
            show_mark(badge, mark(id));
        }
    }

    fn set_sync_activity(self: &Rc<Self>, root: Option<DriveFileId>) {
        if *self.active_root.borrow() != root {
            self.active_root.replace(root);
            self.refresh();
        }
    }

    fn index_choice(&self) -> aerie_store::Indexing {
        match self.state.borrow().catalog() {
            AppCatalog::Drive(catalog) => catalog.indexing(),
            AppCatalog::Samples(_) => aerie_store::Indexing {
                all: true,
                drives: Vec::new(),
            },
        }
    }

    fn indexing_choice(self: &Rc<Self>) -> Option<crate::preferences::IndexingChoice> {
        let drives = match self.state.borrow().catalog() {
            AppCatalog::Drive(catalog) => catalog.shared_drives(),
            AppCatalog::Samples(_) => return None,
        };
        let engine = self.engine.clone();
        Some(crate::preferences::IndexingChoice {
            current: self.index_choice(),
            drives,
            apply: Box::new(move |indexing| engine.send(Command::SetIndexing(indexing))),
        })
    }

    /// Fetches the open folder when it's in a shared drive that isn't indexed.
    fn load_if_unindexed(&self) {
        let choice = self.index_choice();
        let state = self.state.borrow();
        let folder = state.current_folder().clone();
        let drive = state.catalog().get(&folder).and_then(|entry| entry.drive);
        drop(state);
        match drive {
            Some(drive) if !choice.all && !choice.drives.contains(&drive) => {
                self.loading_folder.replace(Some(folder.clone()));
                self.engine.send(Command::BrowseFolder { drive, folder });
            }
            _ => {
                self.loading_folder.replace(None);
            }
        }
    }

    /// A folder fetched on demand arrived.
    fn folder_loaded(self: &Rc<Self>, folder: &DriveFileId) {
        if self.loading_folder.borrow().as_ref() == Some(folder) {
            self.loading_folder.replace(None);
            self.refresh();
        }
    }

    /// Refreshes after navigation, which ends any search.
    fn show_state(self: &Rc<Self>) {
        self.load_if_unindexed();
        if self.search.text().is_empty() {
            self.refresh();
        } else {
            // Emits search-changed, which refreshes.
            self.search.set_text("");
        }
    }

    fn refresh(self: &Rc<Self>) {
        let state = self.state.borrow();
        let searching = state.is_searching();
        let items: Vec<glib::BoxedAnyObject> = state
            .visible()
            .into_iter()
            .map(|entry| {
                let mark = if entry.is_folder() {
                    self.mark(state.catalog(), &entry.id)
                } else {
                    SyncMark::None
                };
                glib::BoxedAnyObject::new(Row { entry, mark })
            })
            .collect();
        let crumb_mark = self.mark(state.catalog(), state.current_folder());
        drop(state);
        self.crumb_mark.set(crumb_mark);
        self.replace_rows(&items);

        if items.is_empty() {
            self.show_empty(searching);
            self.content.set_visible_child_name("empty");
        } else {
            self.content.set_visible_child_name("list");
        }
        self.rebuild_breadcrumbs(searching);
        self.update_actions();
        self.update_sidebar_marks();
    }

    /// Swaps in new rows, touching only the run that changed. The list refreshes after
    /// every page of an index, and replacing everything would drop the selection, close
    /// the context menu and scroll back to the top.
    fn replace_rows(&self, items: &[glib::BoxedAnyObject]) {
        let current: Vec<glib::Object> = (0..self.store.n_items())
            .filter_map(|position| self.store.item(position))
            .collect();
        let same = |old: &glib::Object, new: &glib::BoxedAnyObject| {
            let (old, new) = (row_of(old), new.borrow::<Row>());
            old.entry == new.entry && old.mark == new.mark
        };
        let prefix = current
            .iter()
            .zip(items)
            .take_while(|(old, new)| same(old, new))
            .count();
        let suffix = current[prefix..]
            .iter()
            .rev()
            .zip(items[prefix..].iter().rev())
            .take_while(|(old, new)| same(old, new))
            .count();
        let removed = current.len() - prefix - suffix;
        let added = &items[prefix..items.len() - suffix];
        if removed == 0 && added.is_empty() {
            return;
        }
        self.store.splice(prefix as u32, removed as u32, added);
    }

    /// An unfinished inventory must never look like an empty Drive.
    fn show_empty(&self, searching: bool) {
        if !searching && self.loading_folder.borrow().is_some() {
            self.empty
                .set_paintable(Some(&adw::SpinnerPaintable::new(Some(&self.empty))));
            self.empty.set_title("Loading folder");
            self.empty.set_description(Some(
                "This shared drive isn’t indexed, so its folders load when you open them.",
            ));
        } else if self.indexing.get() {
            self.empty
                .set_paintable(Some(&adw::SpinnerPaintable::new(Some(&self.empty))));
            self.empty.set_title("Indexing your Drive");
            self.empty.set_description(Some(
                "Files appear here as Aerie finds them. This happens once.",
            ));
        } else if searching {
            self.empty.set_icon_name(Some("aerie-search-symbolic"));
            self.empty.set_title("No results");
            self.empty
                .set_description(Some("Try a different file name."));
        } else {
            self.empty.set_icon_name(Some("aerie-folder-symbolic"));
            self.empty.set_title("This folder is empty");
            self.empty.set_description(None);
        }
    }

    fn rebuild_breadcrumbs(self: &Rc<Self>, searching: bool) {
        while let Some(child) = self.breadcrumbs.first_child() {
            self.breadcrumbs.remove(&child);
        }

        if searching {
            self.breadcrumbs.append(&crumb_label("Search results"));
            return;
        }

        let state = self.state.borrow();
        let last = state.depth();
        let root_label = self.root_label();
        for (depth, name) in std::iter::once(root_label.as_str())
            .chain(state.path_names())
            .enumerate()
        {
            if depth > 0 {
                self.breadcrumbs.append(
                    &gtk::Image::builder()
                        .icon_name("aerie-chevron-right-symbolic")
                        .css_classes(["dim-label"])
                        .accessible_role(gtk::AccessibleRole::Presentation)
                        .build(),
                );
            }
            if depth == last {
                self.breadcrumbs.append(&crumb_label(name));
                let badge = gtk::Image::builder()
                    .css_classes(["sync-badge"])
                    .pixel_size(14)
                    .build();
                show_mark(&badge, self.crumb_mark.get());
                self.breadcrumbs.append(&badge);
                continue;
            }
            let button = gtk::Button::builder()
                .label(name)
                .css_classes(["flat"])
                .build();
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = browser)]
                self,
                move |_| {
                    browser.state.borrow_mut().go_to_depth(depth);
                    browser.show_state();
                }
            ));
            self.breadcrumbs.append(&button);
        }
    }

    fn show_new_folder_dialog(self: &Rc<Self>) {
        let entry = gtk::Entry::builder()
            .placeholder_text("Folder name")
            .activates_default(true)
            .build();
        let dialog = adw::AlertDialog::builder()
            .heading("New folder")
            .extra_child(&entry)
            .close_response("cancel")
            .default_response("create")
            .build();
        dialog.add_responses(&[("cancel", "Cancel"), ("create", "Create")]);
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("create", false);

        entry.connect_changed(glib::clone!(
            #[weak]
            dialog,
            move |entry| dialog.set_response_enabled("create", !entry.text().trim().is_empty())
        ));
        dialog.connect_response(
            Some("create"),
            glib::clone!(
                #[weak(rename_to = browser)]
                self,
                #[weak]
                entry,
                move |_, _| browser.create_folder(entry.text().trim())
            ),
        );
        dialog.present(Some(&self.view));
        entry.grab_focus();
    }

    fn create_folder(self: &Rc<Self>, name: &str) {
        if self.is_drive() {
            let parent = self.state.borrow().current_folder().clone();
            self.engine.send(Command::CreateFolder {
                parent,
                name: name.to_owned(),
            });
            return;
        }
        self.create_sample_folder(name);
    }

    fn create_sample_folder(self: &Rc<Self>, name: &str) {
        let id = DriveFileId::new(format!("local-{}", self.next_local_id.get()));
        self.next_local_id.set(self.next_local_id.get() + 1);
        {
            let mut state = self.state.borrow_mut();
            let parent = state.current_folder().clone();
            let AppCatalog::Samples(catalog) = state.catalog_mut() else {
                return;
            };
            catalog.apply(aerie_core::Change::Upsert(FileEntry {
                id,
                parent: Some(parent),
                name: name.to_owned(),
                kind: FileKind::Folder,
                size: None,
                modified: glib::DateTime::now_local().map_or(0, |now| now.to_unix()),
                mime_type: String::new(),
                md5: None,
                head_revision: None,
                drive: None,
                can: aerie_core::Capabilities::ALL,
            }));
            let depth = state.depth();
            state.go_to_depth(depth);
        }
        self.show_state();
    }
}

fn samples() -> BrowseState<AppCatalog> {
    BrowseState::new(
        DriveFileId::new(ROOT_ID),
        AppCatalog::Samples(MemoryCatalog::new(fixtures::sample_entries())),
    )
}

/// Above this many shared drives, the sidebar offers a filter.
const FILTER_DRIVES_ABOVE: usize = 10;

/// One of eight hues, the same for a drive every time, so drives are told apart at a glance.
fn drive_hue(id: &DriveFileId) -> &'static str {
    const HUES: [&str; 8] = [
        "hue-0", "hue-1", "hue-2", "hue-3", "hue-4", "hue-5", "hue-6", "hue-7",
    ];
    let hash = id.as_str().bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    HUES[hash as usize % HUES.len()]
}

const SIDEBAR_WIDTHS: std::ops::RangeInclusive<f64> = 180.0..=480.0;

/// Fixes the sidebar at `width`; the split view sizes it between its minimum and maximum.
fn set_sidebar_width(split: &adw::NavigationSplitView, width: f64) {
    let width = width.clamp(*SIDEBAR_WIDTHS.start(), *SIDEBAR_WIDTHS.end());
    // Widths are measured in pixels, which the default unit isn't under text scaling.
    split.set_sidebar_width_unit(adw::LengthUnit::Px);
    split.set_min_sidebar_width(width);
    split.set_max_sidebar_width(width);
}

/// A thin strip on the sidebar's edge that drags its width, which is remembered.
/// Double-clicking it restores the default.
fn resize_handle(split: &adw::NavigationSplitView, sidebar: &gtk::Overlay) -> gtk::Widget {
    let handle = gtk::Box::builder()
        .width_request(8)
        .halign(gtk::Align::End)
        .css_classes(["sidebar-resize"])
        .cursor(&gtk::gdk::Cursor::from_name("col-resize", None).expect("a standard cursor"))
        .tooltip_text("Drag to resize")
        .build();
    let drag = gtk::GestureDrag::new();
    let start = std::rc::Rc::new(std::cell::Cell::new(0.0));
    drag.connect_drag_begin(glib::clone!(
        #[weak]
        sidebar,
        #[strong]
        start,
        move |_, _, _| {
            tracing::debug!(width = sidebar.width(), "resizing the sidebar");
            start.set(f64::from(sidebar.width()));
        }
    ));
    drag.connect_drag_update(glib::clone!(
        #[weak]
        split,
        #[strong]
        start,
        move |_, dx, _| set_sidebar_width(&split, start.get() + dx)
    ));
    drag.connect_drag_end(glib::clone!(
        #[weak]
        split,
        move |_, _, _| {
            let mut settings = crate::preferences::Settings::load();
            settings.sidebar_width = Some(split.min_sidebar_width());
            settings.save();
        }
    ));
    handle.add_controller(drag);

    let reset = gtk::GestureClick::new();
    reset.connect_pressed(glib::clone!(
        #[weak]
        split,
        move |_, presses, _, _| {
            if presses == 2 {
                split.set_sidebar_width_unit(adw::LengthUnit::Sp);
                split.set_min_sidebar_width(200.0);
                split.set_max_sidebar_width(260.0);
                let mut settings = crate::preferences::Settings::load();
                settings.sidebar_width = None;
                settings.save();
            }
        }
    ));
    handle.add_controller(reset);
    handle.upcast()
}

fn sidebar_row(icon: &str, label: &str) -> gtk::ListBoxRow {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.append(&gtk::Image::from_icon_name(icon));
    row.append(&gtk::Label::new(Some(label)));
    gtk::ListBoxRow::builder().child(&row).build()
}

fn crumb_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["current"])
        .build()
}

fn column(
    title: &str,
    factory: gtk::SignalListItemFactory,
    sorter: gtk::CustomSorter,
) -> gtk::ColumnViewColumn {
    gtk::ColumnViewColumn::builder()
        .title(title)
        .factory(&factory)
        .sorter(&sorter)
        .resizable(true)
        .build()
}

fn by(compare: impl Fn(&FileEntry, &FileEntry) -> Ordering + 'static) -> gtk::CustomSorter {
    gtk::CustomSorter::new(move |a, b| {
        let a = entry_of(a);
        let b = entry_of(b);
        compare(&a, &b).into()
    })
}

/// Whether a folder syncs to this computer, and whether a pass is working in it now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncMark {
    None,
    Synced,
    Syncing,
}

/// One row of the file list.
struct Row {
    entry: FileEntry,
    mark: SyncMark,
}

fn row_of(object: &glib::Object) -> std::cell::Ref<'_, Row> {
    object
        .downcast_ref::<glib::BoxedAnyObject>()
        .expect("list items are rows")
        .borrow::<Row>()
}

fn entry_of(object: &glib::Object) -> std::cell::Ref<'_, FileEntry> {
    std::cell::Ref::map(row_of(object), |row| &row.entry)
}

/// The badge beside a synced folder: a gently turning arrow while syncing, a cloud with a
/// check otherwise.
fn sidebar_badge() -> gtk::Image {
    gtk::Image::builder()
        .css_classes(["sync-badge"])
        .pixel_size(14)
        .halign(gtk::Align::End)
        .visible(false)
        .build()
}

fn show_mark(badge: &gtk::Image, mark: SyncMark) {
    badge.set_visible(mark != SyncMark::None);
    match mark {
        SyncMark::Syncing => {
            badge.set_icon_name(Some("aerie-refresh-cw-symbolic"));
            badge.set_tooltip_text(Some("Syncing now"));
            badge.add_css_class("syncing");
        }
        SyncMark::Synced => {
            badge.set_icon_name(Some("aerie-cloud-check-symbolic"));
            badge.set_tooltip_text(Some("Synced to this computer"));
            badge.remove_css_class("syncing");
        }
        SyncMark::None => badge.remove_css_class("syncing"),
    }
    let label = badge.tooltip_text().unwrap_or_default();
    badge.update_property(&[gtk::accessible::Property::Label(&label)]);
}

fn name_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.append(
            &gtk::Image::builder()
                .css_classes(["file-icon"])
                .pixel_size(16)
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
        row.append(
            &gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .build(),
        );
        row.append(
            &gtk::Image::builder()
                .css_classes(["sync-badge"])
                .pixel_size(14)
                .visible(false)
                .build(),
        );
        list_item(item).set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let item = list_item(item);
        let object = item.item().expect("bound item");
        let row_data = row_of(&object);
        let entry = &row_data.entry;
        let row = item.child().expect("row widget");
        let icon = row
            .first_child()
            .and_downcast::<gtk::Image>()
            .expect("icon");
        let label = icon
            .next_sibling()
            .and_downcast::<gtk::Label>()
            .expect("label");
        let badge = label
            .next_sibling()
            .and_downcast::<gtk::Image>()
            .expect("badge");
        show_mark(&badge, row_data.mark);
        icon.set_icon_name(Some(icon_name(entry.kind)));
        icon.set_css_classes(&["file-icon", kind_class(entry.kind)]);
        label.set_label(&entry.name);
        row.set_widget_name(&format!("{ENTRY_PREFIX}{}", entry.id.as_str()));
    });
    factory
}

/// Cells name themselves after their entry so a right-click can find its row.
const ENTRY_PREFIX: &str = "entry:";

fn drive_url(entry: &FileEntry) -> String {
    format!("https://drive.google.com/open?id={}", entry.id.as_str())
}

/// Where a metadata column sets its text: dates read from the left, sizes line up on
/// the right.
#[derive(Clone, Copy)]
enum Align {
    Start,
    End,
}

fn text_factory(
    align: Align,
    text: impl Fn(&FileEntry) -> String + 'static,
    tooltip: impl Fn(&FileEntry) -> Option<String> + 'static,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let label = gtk::Label::builder()
            .css_classes(["metadata", "numeric"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        match align {
            Align::Start => label.set_xalign(0.0),
            Align::End => {
                label.set_xalign(1.0);
                label.set_margin_end(14);
            }
        }
        list_item(item).set_child(Some(&label));
    });
    factory.connect_bind(move |_, item| {
        let item = list_item(item);
        let object = item.item().expect("bound item");
        let entry = entry_of(&object);
        let label = item.child().and_downcast::<gtk::Label>().expect("label");
        label.set_label(&text(&entry));
        label.set_tooltip_text(tooltip(&entry).as_deref());
        label.set_widget_name(&format!("{ENTRY_PREFIX}{}", entry.id.as_str()));
    });
    factory
}

fn list_item(object: &glib::Object) -> &gtk::ListItem {
    object
        .downcast_ref::<gtk::ListItem>()
        .expect("column items are list items")
}

fn icon_name(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Folder => "aerie-folder-fill-symbolic",
        FileKind::Document | FileKind::Pdf => "aerie-file-text-symbolic",
        FileKind::Spreadsheet => "aerie-file-spreadsheet-symbolic",
        FileKind::Presentation => "aerie-presentation-symbolic",
        FileKind::Image => "aerie-file-image-symbolic",
        FileKind::Other => "aerie-file-symbolic",
    }
}

/// The style class that colors a file's icon tile by its type.
fn kind_class(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Folder => "kind-folder",
        FileKind::Document => "kind-document",
        FileKind::Spreadsheet => "kind-spreadsheet",
        FileKind::Presentation => "kind-presentation",
        FileKind::Pdf => "kind-pdf",
        FileKind::Image => "kind-image",
        FileKind::Other => "kind-other",
    }
}

fn format_modified(unix: i64) -> String {
    match (
        glib::DateTime::from_unix_local(unix),
        glib::DateTime::now_local(),
    ) {
        (Ok(time), Ok(now)) => relative_date(&time, &now),
        _ => "—".to_owned(),
    }
}

/// How people say when something changed: minutes and hours today, then "Yesterday",
/// weekdays within the week, and the date after that (with the year only when it differs).
fn relative_date(time: &glib::DateTime, now: &glib::DateTime) -> String {
    let seconds = now.difference(time).as_seconds();
    let days_apart = |a: &glib::DateTime, b: &glib::DateTime| {
        let day = |t: &glib::DateTime| {
            glib::DateTime::from_local(t.year(), t.month(), t.day_of_month(), 0, 0, 0.0)
                .map_or(0, |midnight| midnight.to_unix() / 86_400)
        };
        day(a) - day(b)
    };
    let format = |pattern: &str| {
        time.format(pattern)
            .map_or_else(|_| "—".to_owned(), String::from)
    };
    match seconds {
        // A clock slightly ahead of Google's still reads sensibly.
        ..60 => "Just now".to_owned(),
        60..3_600 => match seconds / 60 {
            1 => "1 minute ago".to_owned(),
            minutes => format!("{minutes} minutes ago"),
        },
        _ => match days_apart(now, time) {
            0 => match seconds / 3_600 {
                1 => "1 hour ago".to_owned(),
                hours => format!("{hours} hours ago"),
            },
            1 => "Yesterday".to_owned(),
            2..7 => format("%A"),
            _ if time.year() == now.year() => format("%b %-e"),
            _ => format("%b %-e, %Y"),
        },
    }
}

/// The exact time, for tooltips.
fn format_full_date(unix: i64) -> String {
    glib::DateTime::from_unix_local(unix)
        .and_then(|time| time.format("%A, %B %-e, %Y at %-l:%M %p"))
        .map_or_else(|_| String::new(), String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: i32, hour: i32, minute: i32) -> glib::DateTime {
        glib::DateTime::from_local(2026, 10, day, hour, minute, 0.0).unwrap()
    }

    #[test]
    fn dates_read_relative_to_now() {
        let now = at(20, 15, 0);
        assert_eq!(relative_date(&now, &now), "Just now");
        // A clock slightly behind Google's.
        assert_eq!(relative_date(&at(20, 15, 1), &now), "Just now");
        assert_eq!(relative_date(&at(20, 14, 59), &now), "1 minute ago");
        assert_eq!(relative_date(&at(20, 14, 55), &now), "5 minutes ago");
        assert_eq!(relative_date(&at(20, 14, 0), &now), "1 hour ago");
        assert_eq!(relative_date(&at(20, 1, 0), &now), "14 hours ago");
        assert_eq!(relative_date(&at(19, 23, 0), &now), "Yesterday");
        assert_eq!(relative_date(&at(16, 9, 0), &now), "Friday");
        assert_eq!(relative_date(&at(2, 9, 0), &now), "Oct 2");
        let last_year = glib::DateTime::from_local(2025, 12, 31, 9, 0, 0.0).unwrap();
        assert_eq!(relative_date(&last_year, &now), "Dec 31, 2025");
    }
}
