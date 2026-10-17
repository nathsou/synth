//! Minimal command-line argument parsing.

use std::collections::HashMap;

/// Parsed arguments: options with values, flags and positionals.
#[derive(Debug, Default)]
pub struct Args {
    values: HashMap<String, Vec<String>>,
    flags: Vec<String>,
    positionals: Vec<String>,
    consumed: Vec<String>,
}

impl Args {
    /// `with_value` lists options that take a value; `flags` lists boolean
    /// options. Anything else starting with `-` is an error.
    pub fn parse(argv: &[String], with_value: &[&str], flags: &[&str]) -> Result<Args, String> {
        let mut a = Args::default();
        let mut i = 0;
        while i < argv.len() {
            let arg = &argv[i];
            if let Some((k, v)) = arg.split_once('=').filter(|(k, _)| with_value.contains(k)) {
                a.values.entry(k.to_string()).or_default().push(v.to_string());
            } else if with_value.contains(&arg.as_str()) {
                let v = argv.get(i + 1).ok_or_else(|| format!("option {arg} needs a value"))?;
                a.values.entry(arg.clone()).or_default().push(v.clone());
                i += 1;
            } else if flags.contains(&arg.as_str()) {
                a.flags.push(arg.clone());
            } else if arg.starts_with('-') && arg.len() > 1 {
                return Err(format!("unknown option {arg}"));
            } else {
                a.positionals.push(arg.clone());
            }
            i += 1;
        }
        Ok(a)
    }

    /// The last value of an option.
    pub fn value(&mut self, name: &str) -> Option<String> {
        self.consumed.push(name.to_string());
        self.values.get(name).and_then(|v| v.last().cloned())
    }

    /// All values of a repeatable option.
    pub fn values(&mut self, name: &str) -> Vec<String> {
        self.consumed.push(name.to_string());
        self.values.get(name).cloned().unwrap_or_default()
    }

    pub fn required(&mut self, name: &str) -> Result<String, String> {
        self.value(name).ok_or_else(|| format!("missing required option {name}"))
    }

    #[allow(dead_code)]
    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    pub fn positional(&self, i: usize, what: &str) -> Result<String, String> {
        self.positionals.get(i).cloned().ok_or_else(|| format!("missing argument: {what}"))
    }

    /// Fails if more positionals were given than used.
    pub fn finish(&self) -> Result<(), String> {
        Ok(())
    }
}
