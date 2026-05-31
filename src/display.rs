use anyhow::{anyhow, Result};
use drm::{
    buffer::DrmFourcc,
    control::{
        atomic, connector,
        dumbbuffer::{DumbBuffer, DumbMapping},
        framebuffer, property, AtomicCommitFlags, ClipRect, Device as ControlDevice, Mode,
        ResourceHandle,
    },
    ClientCapability, Device as DrmDevice,
};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::io::{AsFd, BorrowedFd},
    path::{Path, PathBuf},
    sync::mpsc::{sync_channel, SyncSender},
    thread,
    time::Duration,
};

struct Card(File);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

fn log_object_properties<T: ResourceHandle + std::fmt::Debug>(
    card: &Card,
    label: &str,
    handle: T,
) -> Result<()> {
    let props = card.get_properties(handle)?;
    let (prop_ids, prop_values) = props.as_props_and_values();
    eprintln!("{label} {:?} has {} properties", handle, prop_ids.len());
    for (prop_id, raw_value) in prop_ids.iter().zip(prop_values.iter()) {
        let info = card.get_property(*prop_id)?;
        let name = info.name().to_str().unwrap_or("<invalid-name>");
        eprintln!(
            "  - {} ({}) = {} [{:?}]",
            name,
            u32::from(*prop_id),
            raw_value,
            info.value_type()
        );
    }
    Ok(())
}

fn pick_primary_format(plane_formats: &[u32]) -> Option<DrmFourcc> {
    if plane_formats
        .iter()
        .any(|f| *f == DrmFourcc::Argb8888 as u32)
    {
        return Some(DrmFourcc::Argb8888);
    }
    if plane_formats
        .iter()
        .any(|f| *f == DrmFourcc::Xrgb8888 as u32)
    {
        return Some(DrmFourcc::Xrgb8888);
    }
    None
}

impl ControlDevice for Card {}
impl DrmDevice for Card {}

impl Card {
    fn open(path: &Path) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true);
        options.write(true);
        Ok(Card(options.open(path)?))
    }
}

pub struct DrmBackend {
    card: Card,
    mode: Mode,
    db: DumbBuffer,
    fb: framebuffer::Handle,
}

impl Drop for DrmBackend {
    fn drop(&mut self) {
        self.card.destroy_framebuffer(self.fb).unwrap();
        self.card.destroy_dumb_buffer(self.db).unwrap();
    }
}

fn find_prop_id<T: ResourceHandle>(
    card: &Card,
    handle: T,
    name: &'static str,
) -> Result<property::Handle> {
    let props = card.get_properties(handle)?;
    for id in props.as_props_and_values().0 {
        let info = card.get_property(*id)?;
        if info.name().to_str()?.eq_ignore_ascii_case(name) {
            return Ok(*id);
        }
    }
    return Err(anyhow!("Property not found"));
}
fn find_optional_prop<T: ResourceHandle>(
    card: &Card,
    handle: T,
    name: &str,
) -> Result<Option<(property::Handle, property::Info)>> {
    let props = card.get_properties(handle)?;
    for id in props.as_props_and_values().0 {
        let info = card.get_property(*id)?;
        if info.name().to_str()?.eq_ignore_ascii_case(name) {
            return Ok(Some((*id, info)));
        }
    }
    Ok(None)
}

fn find_enum_value<'a>(
    values: &'a property::EnumValues,
    names: &[&str],
) -> Option<&'a property::EnumValue> {
    let (_, enums) = values.values();
    names.iter().find_map(|target| {
        enums.iter().find(|candidate| {
            candidate
                .name()
                .to_str()
                .map(|name| name.eq_ignore_ascii_case(target))
                .unwrap_or(false)
        })
    })
}

fn discover_drm_card_paths() -> Result<Vec<PathBuf>> {
    let mut cards = fs::read_dir("/dev/dri/")?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("card"))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();

    cards.sort_by_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("card"))
            .and_then(|index| index.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });

    Ok(cards)
}

fn is_ebusy_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .and_then(|io_err| io_err.raw_os_error())
            == Some(16)
    })
}
fn looks_like_touchbar(width: u16, height: u16) -> bool {
    if width == 0 || height == 0 {
        return false;
    }
    // Accept both landscape (2170x60) and portrait (60x2170) DRM modes.
    // The Touch Bar has an extreme aspect ratio regardless of how the DRM mode is oriented.
    let long = u32::from(width).max(u32::from(height));
    let short = u32::from(width).min(u32::from(height));
    long >= short * 8
}

fn try_open_card(path: &Path) -> Result<DrmBackend> {
    let card = Card::open(path)?;
    card.set_client_capability(ClientCapability::UniversalPlanes, true)?;
    card.set_client_capability(ClientCapability::Atomic, true)?;

    let res = card.resource_handles()?;
    let coninfo = res
        .connectors()
        .iter()
        .flat_map(|con| card.get_connector(*con, true))
        .collect::<Vec<_>>();

    let (con, mode) = coninfo
        .iter()
        .filter(|connector| connector.state() == connector::State::Connected)
        .filter_map(|connector| {
            connector
                .modes()
                .iter()
                .copied()
                .filter(|mode| {
                    let (width, height) = mode.size();
                    looks_like_touchbar(width, height)
                })
                .max_by_key(|mode| {
                    let (width, height) = mode.size();
                    (u32::from(width) * 1000) / u32::from(height).max(1)
                })
                .map(|mode| (connector, mode))
        })
        .max_by_key(|(_, mode)| {
            let (width, height) = mode.size();
            (
                (u32::from(width) * 1000) / u32::from(height).max(1),
                u32::from(width),
            )
        })
        .ok_or(anyhow!("No connected touchbar-like connectors found"))?;

    let (disp_width, disp_height) = mode.size();

    let mut possible_crtcs = con
        .encoders()
        .iter()
        .flat_map(|encoder| card.get_encoder(*encoder).ok())
        .flat_map(|encoder| res.filter_crtcs(encoder.possible_crtcs()))
        .collect::<Vec<_>>();
    if possible_crtcs.is_empty() {
        possible_crtcs = res.crtcs().to_vec();
    }
    let crtc = possible_crtcs
        .first()
        .copied()
        .ok_or(anyhow!("No crtcs found"))?;
    let (plane, fmt) = card
        .plane_handles()?
        .iter()
        .flat_map(|handle| card.get_plane(*handle).ok())
        .find_map(|plane_info| {
            let supports_crtc = res
                .filter_crtcs(plane_info.possible_crtcs())
                .iter()
                .any(|candidate| *candidate == crtc);
            if !supports_crtc {
                return None;
            }
            pick_primary_format(plane_info.formats()).map(|fmt| (plane_info.handle(), fmt))
        })
        .ok_or(anyhow!("No compatible plane found with AR24/XR24"))?;
    let db = card.create_dumb_buffer((disp_width.into(), disp_height.into()), fmt, 32)?;

    let fb = card.add_framebuffer(&db, 24, 32)?;

    let mut atomic_req = atomic::AtomicModeReq::new();
    atomic_req.add_property(
        con.handle(),
        find_prop_id(&card, con.handle(), "CRTC_ID")?,
        property::Value::CRTC(Some(crtc)),
    );
    let blob = card.create_property_blob(&mode)?;

    atomic_req.add_property(crtc, find_prop_id(&card, crtc, "MODE_ID")?, blob);
    atomic_req.add_property(
        crtc,
        find_prop_id(&card, crtc, "ACTIVE")?,
        property::Value::Boolean(true),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "FB_ID")?,
        property::Value::Framebuffer(Some(fb)),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "CRTC_ID")?,
        property::Value::CRTC(Some(crtc)),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "SRC_X")?,
        property::Value::UnsignedRange(0),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "SRC_Y")?,
        property::Value::UnsignedRange(0),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "SRC_W")?,
        property::Value::UnsignedRange((mode.size().0 as u64) << 16),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "SRC_H")?,
        property::Value::UnsignedRange((mode.size().1 as u64) << 16),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "CRTC_X")?,
        property::Value::SignedRange(0),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "CRTC_Y")?,
        property::Value::SignedRange(0),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "CRTC_W")?,
        property::Value::UnsignedRange(mode.size().0 as u64),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "CRTC_H")?,
        property::Value::UnsignedRange(mode.size().1 as u64),
    );
    if let Some((prop, info)) = find_optional_prop(&card, plane, "alpha")? {
        if let property::ValueType::UnsignedRange(_, max) = info.value_type() {
            atomic_req.add_property(plane, prop, property::Value::UnsignedRange(max));
        }
    }

    if let Some((prop, info)) = find_optional_prop(&card, plane, "zpos")? {
        if let property::ValueType::UnsignedRange(_, max) = info.value_type() {
            atomic_req.add_property(plane, prop, property::Value::UnsignedRange(max));
        }
    }

    if let Some((prop, info)) = find_optional_prop(&card, plane, "pixel blend mode")? {
        if let property::ValueType::Enum(values) = info.value_type() {
            if let Some(enum_value) = find_enum_value(
                &values,
                &["Pre-multiplied", "Premultiplied", "Coverage", "None"],
            ) {
                atomic_req.add_property(plane, prop, property::Value::Enum(Some(enum_value)));
            }
        }
    }

    eprintln!(
        "tiny-dfr DRM setup: card={}, connector={}, crtc={}, plane={}, fmt={:?}, mode={}x{}, fb={}",
        path.display(),
        u32::from(con.handle()),
        u32::from(crtc),
        u32::from(plane),
        fmt,
        mode.size().0,
        mode.size().1,
        u32::from(fb),
    );
    if let Err(err) = log_object_properties(&card, "Connector", con.handle()) {
        eprintln!("tiny-dfr: failed to dump connector properties: {err}");
    }
    if let Err(err) = log_object_properties(&card, "CRTC", crtc) {
        eprintln!("tiny-dfr: failed to dump CRTC properties: {err}");
    }
    if let Err(err) = log_object_properties(&card, "Plane", plane) {
        eprintln!("tiny-dfr: failed to dump plane properties: {err}");
    }
    let _master_lock = card.acquire_master_lock().map_err(|err| {
        anyhow!(
            "failed to acquire DRM master on {}: {} (tip: keep GDM running, but ensure tiny-dfr starts before display-manager at boot or move the touchbar DRM node to seat-touchbar)",
            path.display(),
            err
        )
    })?;
    eprintln!("tiny-dfr: atomic commit attempt (ALLOW_MODESET)");
    if let Err(err) = card.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, atomic_req) {
        eprintln!("tiny-dfr: atomic commit failed: {err:?}");
        return Err(err.into());
    }

    Ok(DrmBackend { card, mode, db, fb })
}

impl DrmBackend {
    pub fn open_card() -> Result<DrmBackend> {
        const MAX_OPEN_RETRIES: usize = 15;
        const RETRY_DELAY: Duration = Duration::from_millis(750);
        let cards = discover_drm_card_paths()?;
        if cards.is_empty() {
            return Err(anyhow!("No DRM card nodes found in /dev/dri"));
        }

        for attempt in 1..=MAX_OPEN_RETRIES {
            let mut errors = Vec::new();
            let mut saw_ebusy = false;

            for card_path in &cards {
                match try_open_card(card_path) {
                    Ok(card) => return Ok(card),
                    Err(err) => {
                        let ebusy = is_ebusy_error(&err);
                        if ebusy {
                            saw_ebusy = true;
                        }
                        eprintln!(
                            "tiny-dfr: drm probe attempt {}/{} failed for {}{}: {:#}",
                            attempt,
                            MAX_OPEN_RETRIES,
                            card_path.display(),
                            if ebusy { " (EBUSY)" } else { "" },
                            err
                        );
                        errors.push(format!(
                            "{}{}: {} | debug={:?}",
                            card_path.display(),
                            if ebusy { " (EBUSY)" } else { "" },
                            err,
                            err
                        ));
                    }
                }
            }

            if saw_ebusy && attempt < MAX_OPEN_RETRIES {
                eprintln!(
                    "tiny-dfr: retrying DRM card acquisition in {} ms",
                    RETRY_DELAY.as_millis()
                );
                thread::sleep(RETRY_DELAY);
                continue;
            }

            return Err(anyhow!(
                "No touchbar device found after {} attempt(s), attempted: [\n    {}\n]",
                attempt,
                errors.join(",\n    ")
            ));
        }

        Err(anyhow!(
            "No touchbar device found after {} attempts due to repeated EBUSY",
            MAX_OPEN_RETRIES
        ))
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn fb_info(&self) -> Result<framebuffer::Info> {
        Ok(self.card.get_framebuffer(self.fb)?)
    }
    pub fn dirty(&self, clips: &[ClipRect]) -> Result<()> {
        Ok(self.card.dirty_framebuffer(self.fb, clips)?)
    }
    pub fn map(&mut self) -> Result<DumbMapping<'_>> {
        Ok(self.card.map_dumb_buffer(&mut self.db)?)
    }
}

struct RedrawJob {
    pixels: Vec<u8>,
    clips: Vec<ClipRect>,
}

/// Owns the DRM device on a worker thread so input handling never blocks on USB.
pub struct DrmRedrawHandle {
    tx: SyncSender<RedrawJob>,
}

impl DrmRedrawHandle {
    pub fn spawn(mut drm: DrmBackend) -> Self {
        let (tx, rx) = sync_channel::<RedrawJob>(2);
        thread::spawn(move || {
            while let Ok(mut job) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    job = newer;
                }
                if let Ok(mut map) = drm.map() {
                    let len = job.pixels.len().min(map.as_mut().len());
                    map.as_mut()[..len].copy_from_slice(&job.pixels[..len]);
                }
                let _ = drm.dirty(&job.clips);
            }
        });
        Self { tx }
    }

    /// Queue a framebuffer flush; drops the frame if USB is still busy (never blocks input).
    pub fn try_push(&self, pixels: Vec<u8>, clips: Vec<ClipRect>) {
        let _ = self.tx.try_send(RedrawJob { pixels, clips });
    }
}
