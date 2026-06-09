use std::path::PathBuf;

pub struct Config {
    pub db_path: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        let db_path = if let Ok(p) = std::env::var("OL_DB") {
            PathBuf::from(p)
        } else {
            let home = dirs::home_dir().expect("could not find home directory");
            home.join(".ol").join("ol.db")
        };
        Self { db_path }
    }
}
