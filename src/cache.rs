use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf};
pub struct Cache {
    path: PathBuf,
    udn: String,
    entries: BTreeMap<u64, Value>,
}
impl Cache {
    pub fn load(directory: &std::path::Path, udn: &str) -> Self {
        let path = directory.join("programs.json");
        let mut cache = Self {
            path,
            udn: udn.into(),
            entries: BTreeMap::new(),
        };
        if std::fs::metadata(&cache.path).is_ok_and(|m| m.len() <= 4 * 1024 * 1024) {
            if let Ok(v) = std::fs::read(&cache.path)
                .and_then(|b| serde_json::from_slice::<Value>(&b).map_err(std::io::Error::other))
            {
                if v["udn"].as_str() == Some(udn) {
                    if let Some(items) = v["programs"].as_array().filter(|a| a.len() <= 2000) {
                        for item in items {
                            let _ = cache.update(item.clone());
                        }
                    }
                }
            }
        }
        cache
    }
    pub fn update(&mut self, p: Value) -> Result<bool> {
        let nid = p["networkId"]
            .as_u64()
            .filter(|n| *n <= 65535)
            .ok_or_else(|| anyhow::anyhow!("Invalid EPG network"))?;
        let sid = p["serviceId"]
            .as_u64()
            .filter(|n| *n <= 65535)
            .ok_or_else(|| anyhow::anyhow!("Invalid EPG service"))?;
        ensure!(
            p["eventId"].as_u64().is_some_and(|n| n <= 65535)
                && p["startAt"]
                    .as_u64()
                    .is_some_and(|n| (946684800000..=4102444800000).contains(&n))
                && p["duration"]
                    .as_u64()
                    .is_some_and(|n| n > 0 && n <= 7 * 86400000)
                && p["name"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty() && s.len() <= 65536),
            "Invalid EPG event"
        );
        let key = nid * 100000 + sid;
        let changed = self.entries.get(&key) != Some(&p);
        if changed {
            self.entries.insert(key, p);
        }
        Ok(changed)
    }
    pub fn programs(&self) -> Vec<Value> {
        let oldest = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .saturating_sub(6 * 3600000) as u64;
        self.entries
            .values()
            .filter(|p| {
                p["startAt"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(p["duration"].as_u64().unwrap_or(0))
                    > oldest
            })
            .cloned()
            .collect()
    }
    pub fn save(&self) -> Result<()> {
        if let Some(p) = self.path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec(&json!({"udn":self.udn,"programs":self.programs()}))?,
        )?;
        std::fs::rename(tmp, &self.path)?;
        Ok(())
    }
}
