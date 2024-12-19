use std::{
    fs::{File, OpenOptions},
    io::{self, Read as _, Seek, Write as _},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct Config {
    path: PathBuf,
}

impl Config {
    pub fn new(dir: &Path) -> Self {
        let path = dir.join("config.toml");
        Self { path }
    }

    fn config_file(&self) -> anyhow::Result<File> {
        // We do create the file it it does not exist, but this will fail if the directory
        // does not exist.
        let ret = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(&self.path)?;

        Ok(ret)
    }

    fn edit_config(
        &self,
        func: impl FnOnce(toml::Table) -> anyhow::Result<toml::Table>,
    ) -> anyhow::Result<()> {
        let mut f = self.config_file()?;
        let config = Self::parse_config(&mut f)?;

        Self::write_config(&mut f, func(config)?)?;

        Ok(())
    }

    fn get_config(&self) -> anyhow::Result<toml::Table> {
        let mut f = self.config_file()?;
        Self::parse_config(&mut f)
    }

    pub fn edit_backend_config<C>(&self, func: impl FnOnce(Option<C>) -> anyhow::Result<C>) -> anyhow::Result<()>
    where
        C: serde::de::DeserializeOwned + serde::Serialize,
    {
        self.edit_config(move |mut config| {
            // We can do this without cloning :)
            let typed_conf = match config.remove("backend") {
                Some(conf) => Some(conf.try_into()?),
                None => None
            };

            let res = func(typed_conf)?;

            let untyped_conf = toml::Value::try_from(res)?;
            config.insert("backend".to_owned(), untyped_conf);

            Ok(config)
        })?;

        Ok(())
    }

    /// Note that this will create the config it it previously did not exist.
    pub fn backend_config<C>(&self) -> anyhow::Result<Option<C>>
    where
        C: serde::de::DeserializeOwned,
    {
        let mut config = self.get_config()?;
        let ret: C = match config.remove("backend") {
            Some(config) => config.try_into()?,
            None => return Ok(None)
        };

        Ok(Some(ret))
    }

    fn parse_config(f: &mut File) -> anyhow::Result<toml::Table> {
        let mut text = String::new();
        f.read_to_string(&mut text)?;

        Ok(toml::from_str(&text)?)
    }

    fn write_config(f: &mut File, val: toml::Table) -> anyhow::Result<()> {
        f.seek(io::SeekFrom::Start(0))?;
        f.set_len(0)?;

        let text = toml::to_string_pretty(&val)?;
        f.write_all(text.as_bytes())?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use assert_matches::assert_matches;
    use serde::{Deserialize, Serialize};
    use tempfile::NamedTempFile;

    #[derive(Deserialize, Serialize, Debug, PartialEq, Clone)]
    struct SampleConfig {
        name: String,
        expiration_time: u64,
    }

    #[test]
    fn read_write_config() {
        let tempf = NamedTempFile::new().unwrap();
        let mgr = super::Config {
            path: tempf.path().to_owned(),
        };

        // Initially, there should be nothing
        assert_matches!(mgr.backend_config::<SampleConfig>(), Ok(None));

        let expected = SampleConfig {
            name: "Hallo".to_owned(),
            expiration_time: 1209812398,
        };

        let exp = expected.clone();
        mgr.edit_backend_config(move |c| {
            // Initially, config is none
            assert!(c.is_none());
            Ok(exp)
        }).unwrap();

        assert_eq!(mgr.backend_config().unwrap(), Some(expected));
    }
}
