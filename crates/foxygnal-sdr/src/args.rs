//! Device strings: `driver:key=value,key=value`.
//!
//! - `""`: the first device discovery finds
//! - `rtlsdr` or `rtlsdr:`: the first RTL-SDR
//! - `rtlsdr:serial=00000001`
//! - `rtltcp:host=10.0.0.5,port=1234`
//! - `soapy:driver=plutosdr,uri=ip:192.168.2.1` (keys after the driver are
//!   passed to SoapySDR as they are)
//!
//! Values may contain `:` and `=`; only `,` separates pairs (write `\,` for a
//! literal comma).

use crate::{Result, SdrError};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceArgs {
    driver: Option<String>,
    pairs: Vec<(String, String)>,
}

impl DeviceArgs {
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(Self::default());
        }
        let (driver, rest) = match s.split_once(':') {
            Some((d, r)) => (d.trim(), r),
            None => (s, ""),
        };
        if driver.is_empty()
            || !driver
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(SdrError::BadArgs(
                s.into(),
                "expected `driver:key=value,…` (e.g. `rtlsdr:serial=00000001`)".into(),
            ));
        }
        let mut pairs = Vec::new();
        for part in split_unescaped(rest) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            let k = k.trim();
            if k.is_empty() {
                return Err(SdrError::BadArgs(s.into(), format!("`{part}` has no key")));
            }
            pairs.push((k.to_string(), v.trim().to_string()));
        }
        Ok(DeviceArgs {
            driver: Some(driver.to_string()),
            pairs,
        })
    }

    pub fn driver(&self) -> Option<&str> {
        self.driver.as_deref()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// A key parsed as a number, with an error naming the key.
    pub fn get_parsed<T: std::str::FromStr>(&self, key: &str) -> Result<Option<T>> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(|_| {
                SdrError::BadArgs(
                    self.to_string(),
                    format!("`{key}={v}` is not a valid number"),
                )
            }),
        }
    }

    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    pub fn with(mut self, key: &str, value: impl Into<String>) -> Self {
        self.pairs.retain(|(k, _)| k != key);
        self.pairs.push((key.to_string(), value.into()));
        self
    }

    pub fn new(driver: &str) -> Self {
        DeviceArgs {
            driver: Some(driver.to_string()),
            pairs: vec![],
        }
    }
}

impl std::fmt::Display for DeviceArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Some(d) = &self.driver else {
            return Ok(());
        };
        write!(f, "{d}:")?;
        for (i, (k, v)) in self.pairs.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{k}={}", v.replace(',', "\\,"))?;
        }
        Ok(())
    }
}

fn split_unescaped(s: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&',') => {
                out.last_mut().unwrap().push(',');
                chars.next();
            }
            ',' => out.push(String::new()),
            c => out.last_mut().unwrap().push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_strings() {
        assert_eq!(DeviceArgs::parse("").unwrap().driver(), None);
        let a = DeviceArgs::parse("rtlsdr").unwrap();
        assert_eq!(a.driver(), Some("rtlsdr"));
        assert!(a.pairs().is_empty());
        let a = DeviceArgs::parse("soapy:driver=plutosdr, uri=ip:192.168.2.1").unwrap();
        assert_eq!(a.driver(), Some("soapy"));
        assert_eq!(a.get("driver"), Some("plutosdr"));
        assert_eq!(a.get("uri"), Some("ip:192.168.2.1"));
        let a = DeviceArgs::parse(r"x:label=a\,b,port=5").unwrap();
        assert_eq!(a.get("label"), Some("a,b"));
        assert_eq!(a.get_parsed::<u16>("port").unwrap(), Some(5));
        assert!(a.get_parsed::<u16>("label").is_err());
        assert_eq!(DeviceArgs::parse(&a.to_string()).unwrap(), a);
        assert!(DeviceArgs::parse("not a driver:x").is_err());
        assert!(DeviceArgs::parse("d:=v").is_err());
    }
}
