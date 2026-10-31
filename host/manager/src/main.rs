//! `innkeeper-manager`: the Innkeeper manager. Install Android apps by drag and
//! drop or from the file manager, open and remove them, and watch the
//! session. A GTK 4 / libadwaita front end over `innkeeper-core`; see
//! host/manager/README.md.

mod app;
mod errors;
mod install;
mod permissions;
mod preferences;
mod profiles;
mod window;
mod worker;

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use innkeeper_core::desktop::{MANAGER_ICON_SVG, MANAGER_ID};
use innkeeper_core::xdg::Dirs;

use crate::window::Window;

const CSS: &str = "
.status-dot { min-width: 10px; min-height: 10px; border-radius: 5px;
  background-color: alpha(currentColor, 0.25); }
.status-dot.running { background-color: var(--success-color); }
.status-dot.starting { background-color: var(--warning-color); }
.drop-hint { margin: 12px; border-radius: 18px;
  border: 2px dashed var(--accent-color);
  background-color: color-mix(in srgb, var(--accent-bg-color) 12%, var(--window-bg-color) 88%); }
";

/// Makes the manager's icon findable by name even when it isn't installed
/// (an unthemed icon in a search path).
fn register_icon(display: &gtk::gdk::Display) {
    let dir = Dirs::from_env().cache.join("icons");
    let file = dir.join(format!("{MANAGER_ID}.svg"));
    if std::fs::read_to_string(&file).ok().as_deref() != Some(MANAGER_ICON_SVG) {
        let _ =
            std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&file, MANAGER_ICON_SVG));
    }
    gtk::IconTheme::for_display(display).add_search_path(&dir);
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(MANAGER_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    let window: Rc<RefCell<Option<Window>>> = Rc::default();

    app.connect_startup(|app| {
        let display = gtk::gdk::Display::default().expect("no display");
        register_icon(&display);
        gtk::Window::set_default_icon_name(MANAGER_ID);
        let css = gtk::CssProvider::new();
        css.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        // The profile last chosen, or a first one: an ordinary profile.
        match innkeeper_core::profiles::resolve(true) {
            Ok(p) => {
                let _ = innkeeper_core::paths::select(&p);
            }
            Err(e) => eprintln!("innkeeper-manager: {e:#}"),
        }
        app.set_accels_for_action("win.preferences", &["<Control>comma"]);
        app.set_accels_for_action("win.open", &["<Control>o"]);
        app.set_accels_for_action("app.quit", &["<Control>q"]);
        app.set_accels_for_action("window.close", &["<Control>w"]);
        let quit = gio::SimpleAction::new("quit", None);
        let a = app.clone();
        quit.connect_activate(move |_, _| a.quit());
        app.add_action(&quit);
    });

    let get = {
        let window = window.clone();
        move |app: &adw::Application| -> Window {
            let mut slot = window.borrow_mut();
            if let Some(w) = slot.as_ref()
                && w.win.application().is_some()
            {
                return w.clone();
            }
            let w = Window::new(app);
            let about = gio::SimpleAction::new("about", None);
            let w2 = w.clone();
            about.connect_activate(move |_, _| about_dialog(&w2));
            app.add_action(&about);
            *slot = Some(w.clone());
            w
        }
    };

    let g = get.clone();
    app.connect_activate(move |app| g(app).present());
    app.connect_open(move |app, files, _| {
        let w = get(app);
        w.present();
        for f in files {
            w.open_file(f);
        }
    });
    app.run()
}

fn about_dialog(w: &Window) {
    adw::AboutDialog::builder()
        .application_name("Innkeeper")
        .application_icon(MANAGER_ID)
        .developer_name("Innkeeper contributors")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("Run Android apps on Linux")
        .website("https://github.com/sargunv/innkeeper")
        .issue_url("https://github.com/sargunv/innkeeper/issues")
        .license_type(gtk::License::Agpl30)
        .build()
        .present(Some(&w.win));
}
