//! Walkman Sync — library copy, likes, and scrobbles for Sony Walkmans.

pub mod adb;
pub mod art;
pub mod audiomuse;
pub mod catalog;
pub mod copy;
pub mod keys;
pub mod launch;
pub mod likes;
pub mod manifest;
pub mod paths;
pub mod storage;
pub mod sync;
pub mod tui;
pub mod walkman;

pub fn run(args: &[String]) -> i32 {
    paths::load_dotenv();
    if args.iter().any(|a| a == "--install-shortcut") {
        match launch::install_shortcut() {
            Ok(paths) => {
                for path in &paths {
                    println!("{}", path.display());
                }
                if cfg!(windows) {
                    println!("Pin Walkman Sync from the Start Menu or the desktop.");
                } else {
                    println!(
                        "Double-click Walkman Sync on the Desktop, or drag it from Applications to the Dock."
                    );
                }
                0
            }
            Err(err) => {
                eprintln!("could not install the shortcut: {err}");
                1
            }
        }
    } else if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Walkman Sync\n\n  walkman-sync                 open the dashboard\n  walkman-sync --install-shortcut   Desktop + Start Menu / Applications\n"
        );
        0
    } else {
        match tui::run() {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("{err}");
                1
            }
        }
    }
}
