use once_cell::sync::Lazy;
use std::fs::{create_dir_all, remove_file, symlink_metadata, File};
use std::io::prelude::*;
use std::net::ToSocketAddrs;
use std::time::{Duration, SystemTime};

use rocket::http::ContentType;
use rocket::response::Content;
use rocket::Route;

use reqwest::{Url, header::HeaderMap, blocking::Client, blocking::Response};

use rocket::http::Cookie;

use regex::Regex;
use soup::prelude::*;

use crate::error::Error;
use crate::CONFIG;
use crate::util::Cached;

pub fn routes() -> Vec<Route> {
    routes![icon]
}

const FALLBACK_ICON: &[u8; 344] = include_bytes!("../static/fallback-icon.png");

const ALLOWED_CHARS: &str = "_-.";

static CLIENT: Lazy<Client> = Lazy::new(|| {
    // Reuse the client between requests
    Client::builder()
        .timeout(Duration::from_secs(CONFIG.icon_download_timeout()))
        .default_headers(_header_map())
        .build()
        .unwrap()
});

fn is_valid_domain(domain: &str) -> bool {
    // Don't allow empty or too big domains or path traversal
    if domain.is_empty() || domain.len() > 255 || domain.contains("..") {
        return false;
    }

    // Only alphanumeric or specific characters
    for c in domain.chars() {
        if !c.is_alphanumeric() && !ALLOWED_CHARS.contains(c) {
            return false;
        }
    }

    true
}

#[get("/<domain>/icon.png")]
fn icon(domain: String) -> Cached<Content<Vec<u8>>> {
    let icon_type = ContentType::new("image", "x-icon");

    if !is_valid_domain(&domain) {
        warn!("Invalid domain: {:#?}", domain);
        return Cached::long(Content(icon_type, FALLBACK_ICON.to_vec()));
    }

    Cached::long(Content(icon_type, get_icon(&domain)))
}

fn check_icon_domain_is_blacklisted(domain: &str) -> bool {
    let mut is_blacklisted = CONFIG.icon_blacklist_non_global_ips()
        && (domain, 0)
            .to_socket_addrs()
            .map(|x| {
                for ip_port in x {
                    if !ip_port.ip().is_global() {
                        warn!("IP {} for domain '{}' is not a global IP!", ip_port.ip(), domain);
                        return true;
                    }
                }
                false
            })
            .unwrap_or(false);

    // Skip the regex check if the previous one is true already
    if !is_blacklisted {
        if let Some(blacklist) = CONFIG.icon_blacklist_regex() {
            let regex = Regex::new(&blacklist).expect("Valid Regex");
            if regex.is_match(&domain) {
                warn!("Blacklisted domain: {:#?} matched {:#?}", domain, blacklist);
                is_blacklisted = true;
            }
        }
    }

    is_blacklisted
}

fn get_icon(domain: &str) -> Vec<u8> {
    let path = format!("{}/{}.png", CONFIG.icon_cache_folder(), domain);

    if let Some(icon) = get_cached_icon(&path) {
        return icon;
    }

    if CONFIG.disable_icon_download() {
        return FALLBACK_ICON.to_vec();
    }

    // Get the icon, or fallback in case of error
    match download_icon(&domain) {
        Ok(icon) => {
            save_icon(&path, &icon);
            icon
        }
        Err(e) => {
            error!("Error downloading icon: {:?}", e);
            let miss_indicator = path + ".miss";
            let empty_icon = Vec::new();
            save_icon(&miss_indicator, &empty_icon);
            FALLBACK_ICON.to_vec()
        }
    }
}

fn get_cached_icon(path: &str) -> Option<Vec<u8>> {
    // Check for expiration of negatively cached copy
    if icon_is_negcached(path) {
        return Some(FALLBACK_ICON.to_vec());
    }

    // Check for expiration of successfully cached copy
    if icon_is_expired(path) {
        return None;
    }

    // Try to read the cached icon, and return it if it exists
    if let Ok(mut f) = File::open(path) {
        let mut buffer = Vec::new();

        if f.read_to_end(&mut buffer).is_ok() {
            return Some(buffer);
        }
    }

    None
}

fn file_is_expired(path: &str, ttl: u64) -> Result<bool, Error> {
    let meta = symlink_metadata(path)?;
    let modified = meta.modified()?;
    let age = SystemTime::now().duration_since(modified)?;

    Ok(ttl > 0 && ttl <= age.as_secs())
}

fn icon_is_negcached(path: &str) -> bool {
    let miss_indicator = path.to_owned() + ".miss";
    let expired = file_is_expired(&miss_indicator, CONFIG.icon_cache_negttl());

    match expired {
        // No longer negatively cached, drop the marker
        Ok(true) => {
            if let Err(e) = remove_file(&miss_indicator) {
                error!("Could not remove negative cache indicator for icon {:?}: {:?}", path, e);
            }
            false
        }
        // The marker hasn't expired yet.
        Ok(false) => true,
        // The marker is missing or inaccessible in some way.
        Err(_) => false,
    }
}

fn icon_is_expired(path: &str) -> bool {
    let expired = file_is_expired(path, CONFIG.icon_cache_ttl());
    expired.unwrap_or(true)
}

#[derive(Debug)]
struct Icon {
    priority: u8,
    href: String,
}

impl Icon {
    const fn new(priority: u8, href: String) -> Self {
        Self { href, priority }
    }
}

/// Returns a Result/Tuple which holds a Vector IconList and a string which holds the cookies from the last response.
/// There will always be a result with a string which will contain https://example.com/favicon.ico and an empty string for the cookies.
/// This does not mean that that location does exists, but it is the default location browser use.
///
/// # Argument
/// * `domain` - A string which holds the domain with extension.
///
/// # Example
/// ```
/// let (mut iconlist, cookie_str) = get_icon_url("github.com")?;
/// let (mut iconlist, cookie_str) = get_icon_url("gitlab.com")?;
/// ```
fn get_icon_url(domain: &str) -> Result<(Vec<Icon>, String), Error> {
    // Default URL with secure and insecure schemes
    let ssldomain = format!("https://{}", domain);
    let httpdomain = format!("http://{}", domain);

    // Create the iconlist
    let mut iconlist: Vec<Icon> = Vec::new();

    // Create the cookie_str to fill it all the cookies from the response
    // These cookies can be used to request/download the favicon image.
    // Some sites have extra security in place with for example XSRF Tokens.
    let mut cookie_str = String::new();

    let resp = get_page(&ssldomain).or_else(|_| get_page(&httpdomain));
    if let Ok(content) = resp {
        // Extract the URL from the respose in case redirects occured (like @ gitlab.com)
        let url = content.url().clone();

        let raw_cookies = content.headers().get_all("set-cookie");
        cookie_str = raw_cookies
            .iter()
            .filter_map(|raw_cookie| raw_cookie.to_str().ok())
            .map(|cookie_str| {
                if let Ok(cookie) = Cookie::parse(cookie_str) {
                    format!("{}={}; ", cookie.name(), cookie.value())
                } else {
                    String::new()
                }
            })
            .collect::<String>();

        // Add the default favicon.ico to the list with the domain the content responded from.
        iconlist.push(Icon::new(35, url.join("/favicon.ico").unwrap().into_string()));

        // 512KB should be more than enough for the HTML, though as we only really need
        // the HTML header, it could potentially be reduced even further
        let limited_reader = content.take(512 * 1024);

        let soup = Soup::from_reader(limited_reader)?;
        // Search for and filter
        let favicons = soup
            .tag("link")
            .attr("rel", Regex::new(r"icon$|apple.*icon")?) // Only use icon rels
            .attr("href", Regex::new(r"(?i)\w+\.(jpg|jpeg|png|ico)(\?.*)?$|^data:image.*base64")?) // Only allow specific extensions
            .find_all();

        // Loop through all the found icons and determine it's priority
        for favicon in favicons {
            let sizes = favicon.get("sizes");
            let href = favicon.get("href").expect("Missing href");
            let full_href = url.join(&href).unwrap().into_string();

            let priority = get_icon_priority(&full_href, sizes);

            iconlist.push(Icon::new(priority, full_href))
        }
    } else {
        // Add the default favicon.ico to the list with just the given domain
        iconlist.push(Icon::new(35, format!("{}/favicon.ico", ssldomain)));
        iconlist.push(Icon::new(35, format!("{}/favicon.ico", httpdomain)));
    }

    // Sort the iconlist by priority
    iconlist.sort_by_key(|x| x.priority);

    // There always is an icon in the list, so no need to check if it exists, and just return the first one
    Ok((iconlist, cookie_str))
}

fn get_page(url: &str) -> Result<Response, Error> {
    get_page_with_cookies(url, "")
}

fn get_page_with_cookies(url: &str, cookie_str: &str) -> Result<Response, Error> {
    if check_icon_domain_is_blacklisted(Url::parse(url).unwrap().host_str().unwrap_or_default()) {
        err!("Favicon rel linked to a non blacklisted domain!");
    }

    if cookie_str.is_empty() {
        CLIENT.get(url).send()?.error_for_status().map_err(Into::into)
    } else {
        CLIENT
            .get(url)
            .header("cookie", cookie_str)
            .send()?
            .error_for_status()
            .map_err(Into::into)
    }
}

/// Returns a Integer with the priority of the type of the icon which to prefer.
/// The lower the number the better.
///
/// # Arguments
/// * `href`  - A string which holds the href value or relative path.
/// * `sizes` - The size of the icon if available as a <width>x<height> value like 32x32.
///
/// # Example
/// ```
/// priority1 = get_icon_priority("http://example.com/path/to/a/favicon.png", "32x32");
/// priority2 = get_icon_priority("https://example.com/path/to/a/favicon.ico", "");
/// ```
fn get_icon_priority(href: &str, sizes: Option<String>) -> u8 {
    // Check if there is a dimension set
    let (width, height) = parse_sizes(sizes);

    // Check if there is a size given
    if width != 0 && height != 0 {
        // Only allow square dimensions
        if width == height {
            // Change priority by given size
            if width == 32 {
                1
            } else if width == 64 {
                2
            } else if width >= 24 && width <= 128 {
                3
            } else if width == 16 {
                4
            } else {
                5
            }
        // There are dimensions available, but the image is not a square
        } else {
            200
        }
    } else {
        // Change priority by file extension
        if href.ends_with(".png") {
            10
        } else if href.ends_with(".jpg") || href.ends_with(".jpeg") {
            20
        } else {
            30
        }
    }
}

/// Returns a Tuple with the width and hight as a seperate value extracted from the sizes attribute
/// It will return 0 for both values if no match has been found.
///
/// # Arguments
/// * `sizes` - The size of the icon if available as a <width>x<height> value like 32x32.
///
/// # Example
/// ```
/// let (width, height) = parse_sizes("64x64"); // (64, 64)
/// let (width, height) = parse_sizes("x128x128"); // (128, 128)
/// let (width, height) = parse_sizes("32"); // (0, 0)
/// ```
fn parse_sizes(sizes: Option<String>) -> (u16, u16) {
    let mut width: u16 = 0;
    let mut height: u16 = 0;

    if let Some(sizes) = sizes {
        match Regex::new(r"(?x)(\d+)\D*(\d+)").unwrap().captures(sizes.trim()) {
            None => {}
            Some(dimensions) => {
                if dimensions.len() >= 3 {
                    width = dimensions[1].parse::<u16>().unwrap_or_default();
                    height = dimensions[2].parse::<u16>().unwrap_or_default();
                }
            }
        }
    }

    (width, height)
}

fn download_icon(domain: &str) -> Result<Vec<u8>, Error> {
    if check_icon_domain_is_blacklisted(domain) {
        err!("Domain is blacklisted", domain)
    }

    let (iconlist, cookie_str) = get_icon_url(&domain)?;

    let mut buffer = Vec::new();

    use data_url::DataUrl;

    for icon in iconlist.iter().take(5) {
        if icon.href.starts_with("data:image") {
            let datauri = DataUrl::process(&icon.href).unwrap();
            // Check if we are able to decode the data uri
            match datauri.decode_to_vec() {
                Ok((body, _fragment)) => {
                    // Also check if the size is atleast 67 bytes, which seems to be the smallest png i could create
                    if body.len() >= 67 {
                        buffer = body;
                        break;
                    }
                }
                _ => warn!("data uri is invalid"),
            };
        } else {
            match get_page_with_cookies(&icon.href, &cookie_str) {
                Ok(mut res) => {
                    info!("Downloaded icon from {}", icon.href);
                    res.copy_to(&mut buffer)?;
                    break;
                }
                Err(_) => info!("Download failed for {}", icon.href),
            };
        }
    }

    if buffer.is_empty() {
        err!("Empty response")
    }

    Ok(buffer)
}

fn save_icon(path: &str, icon: &[u8]) {
    match File::create(path) {
        Ok(mut f) => {
            f.write_all(icon).expect("Error writing icon file");
        }
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
            create_dir_all(&CONFIG.icon_cache_folder()).expect("Error creating icon cache");
        }
        Err(e) => {
            info!("Icon save error: {:?}", e);
        }
    }
}

fn _header_map() -> HeaderMap {
    // Set some default headers for the request.
    // Use a browser like user-agent to make sure most websites will return there correct website.
    use reqwest::header::*;

    macro_rules! headers {
        ($( $name:ident : $value:literal),+ $(,)? ) => {
            let mut headers = HeaderMap::new();
            $( headers.insert($name, HeaderValue::from_static($value)); )+
            headers
        };
    }

    headers! {
        USER_AGENT: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Safari/537.36 Edge/16.16299",
        ACCEPT_LANGUAGE: "en-US,en;q=0.8",
        CACHE_CONTROL: "no-cache",
        PRAGMA: "no-cache",
        ACCEPT: "text/html,application/xhtml+xml,application/xml; q=0.9,image/webp,image/apng,*/*;q=0.8",
    }
}
