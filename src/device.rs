//! Bounded SSDP and read-only ContentDirectory; DHCP addresses are seeds only.
use anyhow::{Result, bail, ensure};
use reqwest::{Client, Method};
use roxmltree::{Document, Node};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};
use url::Url;

#[derive(Deserialize, Clone)]
pub struct Config {
    pub location: String,
    pub udn: String,
    pub source: Ipv4Addr,
    pub credentials: std::path::PathBuf,
    pub state_dir: std::path::PathBuf,
    #[serde(default = "folders")]
    pub folders: Vec<String>,
    #[serde(default = "original")]
    pub quality: String,
    #[serde(default = "loopback")]
    pub listen: Ipv4Addr,
    #[serde(default = "port")]
    pub port: u16,
    #[serde(default = "maximum")]
    pub max_streams: usize,
    #[serde(default)]
    pub recording: Value,
    #[serde(default)]
    pub epg_collection: Value,
}
fn folders() -> Vec<String> {
    vec!["0/tb".into(), "0/bs".into(), "0/remux".into()]
}
fn original() -> String {
    "original".into()
}
fn loopback() -> Ipv4Addr {
    Ipv4Addr::LOCALHOST
}
fn port() -> u16 {
    40772
}
fn maximum() -> usize {
    2
}
pub fn lan_url(value: &str, host: Option<Ipv4Addr>) -> Result<Url> {
    let u = Url::parse(value)?;
    ensure!(
        u.scheme() == "http"
            && u.username().is_empty()
            && u.password().is_none()
            && u.fragment().is_none(),
        "Expected plain HTTP LAN URL without credentials"
    );
    let ip = u
        .host_str()
        .and_then(|s| s.parse::<Ipv4Addr>().ok())
        .ok_or_else(|| anyhow::anyhow!("Expected numeric IPv4 LAN address"))?;
    ensure!(
        ip.is_private() && host.is_none_or(|h| h == ip),
        "Destination left verified device"
    );
    Ok(u)
}
fn xml(data: &str) -> Result<Document<'_>> {
    ensure!(
        !data.to_ascii_uppercase().contains("<!DOCTYPE")
            && !data.to_ascii_uppercase().contains("<!ENTITY"),
        "XML entities are forbidden"
    );
    Ok(Document::parse(data)?)
}
fn field(n: Node<'_, '_>, name: &str) -> String {
    let text = n
        .children()
        .find(|c| c.has_tag_name(name))
        .and_then(|c| c.text())
        .unwrap_or("")
        .to_string();
    static MAP: std::sync::OnceLock<std::collections::HashMap<String, String>> =
        std::sync::OnceLock::new();
    let map = MAP.get_or_init(|| {
        serde_json::from_str(include_str!("../templates/arib-pua.json"))
            .expect("Embedded public ARIB map")
    });
    text.chars()
        .map(|c| {
            map.get(&c.to_string())
                .cloned()
                .unwrap_or_else(|| c.to_string())
        })
        .collect()
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
pub fn client(source: Ipv4Addr) -> Result<Client> {
    Ok(Client::builder()
        .no_proxy()
        .local_address(IpAddr::V4(source))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(8))
        .read_timeout(Duration::from_secs(15))
        .tcp_nodelay(true)
        .build()?)
}
pub async fn discover(source: Ipv4Addr) -> Result<Vec<String>> {
    let s = tokio::net::UdpSocket::bind((source, 0)).await?;
    socket2::SockRef::from(&s).set_multicast_if_v4(&source)?;
    s.set_multicast_ttl_v4(2)?;
    s.send_to(b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:MediaServer:1\r\n\r\n",("239.255.255.250",1900)).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut found = std::collections::BTreeSet::new();
    let mut buf = [0; 8192];
    while found.len() < 32 {
        let result = tokio::time::timeout_at(deadline, s.recv_from(&mut buf)).await;
        let Ok(Ok((n, _))) = result else { break };
        for line in String::from_utf8_lossy(&buf[..n]).lines() {
            if let Some((k, v)) = line.split_once(':') {
                if k.eq_ignore_ascii_case("location") && lan_url(v.trim(), None).is_ok() {
                    found.insert(v.trim().to_string());
                }
            }
        }
    }
    Ok(found.into_iter().collect())
}
#[derive(Clone, Serialize)]
pub struct Item {
    pub object_id: String,
    pub token: String,
    pub title: String,
    pub channel: String,
    pub date: String,
    pub description: String,
    pub duration: Option<String>,
    pub service: Option<Value>,
    pub resources: Vec<Resource>,
}
#[derive(Clone, Serialize)]
pub struct Resource {
    pub url: String,
    pub protocol_info: String,
    pub duration: Option<String>,
    pub attributes: std::collections::BTreeMap<String, String>,
}
pub struct Device {
    pub config: Config,
    pub http: Client,
    pub host: Ipv4Addr,
    pub control: String,
    pub service_type: String,
    pub items: Vec<Item>,
}
impl Device {
    pub fn new(config: Config) -> Result<Self> {
        let http = client(config.source)?;
        Ok(Self {
            config,
            http,
            host: Ipv4Addr::UNSPECIFIED,
            control: String::new(),
            service_type: String::new(),
            items: vec![],
        })
    }
    async fn request(&self, url: &str, method: Method, body: Option<String>) -> Result<String> {
        lan_url(
            url,
            if self.host.is_unspecified() {
                None
            } else {
                Some(self.host)
            },
        )?;
        let mut req = self
            .http
            .request(method, url)
            .timeout(Duration::from_secs(8));
        if let Some(body) = body {
            req = req
                .header("Content-Type", "text/xml; charset=utf-8")
                .header("SOAPAction", format!("\"{}#Browse\"", self.service_type))
                .body(body);
        }
        let mut res = req.send().await?;
        ensure!(res.status() == 200, "Device HTTP request failed");
        let mut data = Vec::new();
        while let Some(chunk) = res.chunk().await? {
            ensure!(
                data.len() + chunk.len() <= 4 * 1024 * 1024,
                "Device XML exceeds limit"
            );
            data.extend(chunk);
        }
        Ok(String::from_utf8(data)?)
    }
    pub async fn identify(&mut self) -> Result<()> {
        self.host = Ipv4Addr::UNSPECIFIED;
        let mut candidates = vec![self.config.location.clone()];
        for attempt in 0..2 {
            for candidate in &candidates {
                if self.try_identify(candidate).await.is_ok() {
                    self.config.location = candidate.clone();
                    return Ok(());
                }
            }
            if attempt == 0 {
                candidates = discover(self.config.source).await?;
            }
        }
        bail!("XA401 UDN/model identity unavailable")
    }
    async fn try_identify(&mut self, url: &str) -> Result<()> {
        let data = self.request(url, Method::GET, None).await?;
        let doc = xml(&data)?;
        let device = doc
            .descendants()
            .find(|n| n.has_tag_name("device"))
            .ok_or_else(|| anyhow::anyhow!("Missing device"))?;
        ensure!(
            field(device, "modelName") == "XA401"
                && field(device, "UDN").eq_ignore_ascii_case(&self.config.udn),
            "Device identity mismatch"
        );
        let base = doc
            .descendants()
            .find(|n| n.has_tag_name("URLBase"))
            .and_then(|n| n.text())
            .unwrap_or(url);
        let service = device
            .descendants()
            .find(|n| {
                n.has_tag_name("service") && field(*n, "serviceType").contains(":ContentDirectory:")
            })
            .ok_or_else(|| anyhow::anyhow!("ContentDirectory missing"))?;
        let host = lan_url(url, None)?.host_str().unwrap().parse()?;
        let control = Url::parse(base)?
            .join(&field(service, "controlURL"))?
            .to_string();
        lan_url(&control, Some(host))?;
        self.host = host;
        self.control = control;
        self.service_type = field(service, "serviceType");
        Ok(())
    }
    async fn browse(
        &self,
        id: &str,
        metadata: bool,
        start: usize,
    ) -> Result<(Vec<Item>, usize, usize)> {
        let flag = if metadata {
            "BrowseMetadata"
        } else {
            "BrowseDirectChildren"
        };
        let body = format!(
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:Browse xmlns:u=\"{}\"><ObjectID>{}</ObjectID><BrowseFlag>{}</BrowseFlag><Filter>*</Filter><StartingIndex>{}</StartingIndex><RequestedCount>{}</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>",
            escape(&self.service_type),
            escape(id),
            flag,
            start,
            if metadata { 1 } else { 100 }
        );
        let data = self
            .request(&self.control, Method::POST, Some(body))
            .await?;
        let doc = xml(&data)?;
        let result = doc
            .descendants()
            .find(|n| n.has_tag_name("Result"))
            .and_then(|n| n.text())
            .ok_or_else(|| anyhow::anyhow!("Browse has no Result"))?;
        let count = doc
            .descendants()
            .find(|n| n.has_tag_name("NumberReturned"))
            .and_then(|n| n.text())
            .unwrap_or("0")
            .parse()?;
        let total = doc
            .descendants()
            .find(|n| n.has_tag_name("TotalMatches"))
            .and_then(|n| n.text())
            .unwrap_or("0")
            .parse()?;
        let didl = xml(result)?;
        let mut items = vec![];
        for n in didl.descendants().filter(|n| n.has_tag_name("item")) {
            let id = n.attribute("id").unwrap_or("").to_string();
            let token = hex::encode(Sha256::digest(id.as_bytes()))[..24].to_string();
            let resources: Vec<Resource> = n
                .children()
                .filter(|n| n.has_tag_name("res"))
                .map(|r| Resource {
                    url: r.text().unwrap_or("").into(),
                    protocol_info: r.attribute("protocolInfo").unwrap_or("").into(),
                    duration: r.attribute("duration").map(str::to_string),
                    attributes: r
                        .attributes()
                        .map(|a| (a.name().to_string(), a.value().to_string()))
                        .collect(),
                })
                .collect();
            let mut service = None;
            if let Some((folder, tail)) = id.split_once("/ch?nid=") {
                if let Some((nid, tail)) = tail.split_once("_tsid=") {
                    if let Some((tsid, sid)) = tail.split_once("_svid=") {
                        if let (Ok(nid), Ok(tsid), Ok(sid)) = (
                            u16::from_str_radix(nid, 16),
                            u16::from_str_radix(tsid, 16),
                            u16::from_str_radix(sid, 16),
                        ) {
                            let number = field(n, "channelNr");
                            let remocon = if number.len() == 5 {
                                number.get(1..3).unwrap_or("0")
                            } else {
                                number.get(..number.len().saturating_sub(2)).unwrap_or("0")
                            }
                            .parse::<u32>()
                            .unwrap_or(0);
                            service = Some(
                                json!({"id":nid as u64*100000+sid as u64,"networkId":nid,"serviceId":sid,"transportStreamId":tsid,"type":1,"name":field(n,"channelName"),"remoteControlKeyId":remocon,"channel":{"type":match folder{"0/tb"=>"GR","0/bs"=>"BS",_=>"CS"},"channel":tsid.to_string()},"hasLogoData":false}),
                            );
                        }
                    }
                }
            }
            let duration = resources
                .iter()
                .find(|r| !r.url.contains("quality="))
                .and_then(|r| r.duration.clone());
            items.push(Item {
                object_id: id,
                token,
                title: field(n, "title"),
                channel: field(n, "channelName"),
                date: field(n, "date"),
                description: {
                    let s = field(n, "description");
                    if s.is_empty() {
                        field(n, "longDescription")
                    } else {
                        s
                    }
                },
                duration,
                service,
                resources,
            });
        }
        Ok((items, count, total))
    }
    pub async fn refresh(&mut self) -> Result<()> {
        self.identify().await?;
        let mut items = vec![];
        for folder in &self.config.folders {
            let mut start = 0;
            for _ in 0..5 {
                let (page, n, total) = self.browse(folder, false, start).await?;
                items.extend(page.into_iter().filter(|i| i.service.is_some()));
                start += n;
                if n == 0 || start >= total {
                    break;
                }
            }
        }
        ensure!(!items.is_empty(), "No live services");
        self.items = items;
        Ok(())
    }
    pub async fn recordings(&mut self) -> Result<Vec<Item>> {
        self.identify().await?;
        let mut items = vec![];
        let mut start = 0;
        for _ in 0..20 {
            let (page, n, total) = self.browse("0/rec/all", false, start).await?;
            items.extend(page.into_iter().filter(|i| {
                i.object_id
                    .strip_prefix("0/rec/all/pvr?id=")
                    .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            }));
            start += n;
            if n == 0 || start >= total {
                break;
            }
        }
        Ok(items)
    }
    pub async fn resource(&mut self, object_id: &str) -> Result<Resource> {
        self.identify().await?;
        let (items, _, _) = self.browse(object_id, true, 0).await?;
        ensure!(
            items.len() == 1 && items[0].object_id == object_id,
            "Media identity changed"
        );
        let resource = items[0]
            .resources
            .iter()
            .find(|r| {
                r.protocol_info.to_lowercase().contains("dtcp")
                    && if self.config.quality == "original" {
                        !r.url.contains("quality=")
                    } else {
                        r.url.contains(&format!("quality={}", self.config.quality))
                    }
            })
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Requested protected quality unavailable"))?;
        lan_url(&resource.url, Some(self.host))?;
        Ok(resource)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_boundary() {
        let host = Some(Ipv4Addr::new(192, 168, 3, 5));
        for url in [
            "http://example.com/",
            "http://127.0.0.1/",
            "https://192.168.3.5/",
            "http://user:password@192.168.3.5/",
            "http://192.168.3.6/",
            "http://192.168.3.5/#fragment",
        ] {
            assert!(lan_url(url, host).is_err());
        }
        assert!(lan_url("http://192.168.3.5:57777/stream?a=1", host).is_ok());
        assert!(xml("<!DOCTYPE x [<!ENTITY e SYSTEM 'file:///etc/passwd'>]><x>&e;</x>").is_err());
    }
}
