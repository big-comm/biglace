use gettextrs::{bind_textdomain_codeset, bindtextdomain, setlocale, textdomain, LocaleCategory};

const TEXTDOMAIN: &str = "biglace";

pub fn init() {
    setlocale(LocaleCategory::LcAll, "");

    let dir = locale_dir();
    let _ = bindtextdomain(TEXTDOMAIN, dir);
    let _ = bind_textdomain_codeset(TEXTDOMAIN, "UTF-8");
    let _ = textdomain(TEXTDOMAIN);
}

fn locale_dir() -> String {
    if let Ok(dir) = std::env::var("BIGLACE_LOCALEDIR") {
        return dir;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let candidate = parent.join("../share/locale");
            if candidate.exists() {
                if let Some(s) = candidate.to_str() {
                    return s.to_string();
                }
            }
        }
    }
    "/usr/share/locale".to_string()
}

/// Translate a literal message via gettext.
#[macro_export]
macro_rules! tr {
    ($msg:expr) => {{
        gettextrs::gettext($msg)
    }};
}

/// Translate then substitute named placeholders like `{name}`.
/// Example: `trf!("Error: {error}", "error" => e)`.
#[macro_export]
macro_rules! trf {
    ($msg:expr, $($key:expr => $val:expr),+ $(,)?) => {{
        let mut s = gettextrs::gettext($msg);
        $(
            s = s.replace(&format!("{{{}}}", $key), &$val.to_string());
        )+
        s
    }};
}
