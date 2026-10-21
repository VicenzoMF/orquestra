//! Navegador na UI: decodifica os quadros JPEG fora da thread de render (só
//! dos agentes cujo navegador está na tela) e traduz mouse/teclado sobre a
//! imagem em `BrowserInput`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use egui::{ColorImage, Key, Pos2, Rect};
use orquestra_core::{
    AgentEvent, AgentId, BrowserFrame, EventReceiver, ImageFormat, KeyAction, KeyInput, Modifiers,
};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, warn};

/// Largura das miniaturas dos cards.
pub const THUMB_WIDTH: u32 = 320;

/// Último quadro decodificado de um agente.
pub struct Decoded {
    pub seq: u64,
    /// Metadados do quadro (para `to_viewport`).
    pub frame: Arc<BrowserFrame>,
    pub size: [usize; 2],
    /// Imagem inteira, ainda não enviada para a textura (o painel a pega).
    pub image: Option<ColorImage>,
    /// Miniatura ainda não enviada (os cards a pegam).
    pub thumb: Option<ColorImage>,
}

#[derive(Default)]
struct Inner {
    /// Agentes cujo navegador está na tela; `true` = imagem inteira (painel).
    wanted: HashMap<AgentId, bool>,
    decoded: HashMap<AgentId, Decoded>,
}

/// Quadros decodificados, compartilhados entre a task e a UI.
#[derive(Default)]
pub struct FrameStore {
    inner: Mutex<Inner>,
}

impl FrameStore {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Define quem está na tela. Agentes que saíram perdem a imagem inteira
    /// (a miniatura fica para o card).
    pub fn set_wanted(&self, wanted: HashMap<AgentId, bool>) {
        let mut inner = self.lock();
        for (id, decoded) in inner.decoded.iter_mut() {
            if !wanted.get(id).copied().unwrap_or(false) {
                decoded.image = None;
            }
        }
        inner.wanted = wanted;
    }

    fn wants(&self, id: AgentId) -> Option<bool> {
        self.lock().wanted.get(&id).copied()
    }

    fn last_seq(&self, id: AgentId) -> Option<u64> {
        self.lock().decoded.get(&id).map(|d| d.seq)
    }

    fn put(&self, id: AgentId, decoded: Decoded) {
        self.lock().decoded.insert(id, decoded);
    }

    /// Imagem inteira nova (seq maior que `have`), para a textura do painel.
    pub fn take_image(&self, id: AgentId) -> Option<(u64, ColorImage)> {
        let mut inner = self.lock();
        let d = inner.decoded.get_mut(&id)?;
        d.image.take().map(|img| (d.seq, img))
    }

    pub fn take_thumb(&self, id: AgentId) -> Option<ColorImage> {
        self.lock().decoded.get_mut(&id)?.thumb.take()
    }

    /// Metadados do último quadro (para mapear coordenadas).
    pub fn frame(&self, id: AgentId) -> Option<Arc<BrowserFrame>> {
        self.lock().decoded.get(&id).map(|d| d.frame.clone())
    }

    pub fn forget(&self, keep: &HashSet<AgentId>) {
        self.lock().decoded.retain(|id, _| keep.contains(id));
    }
}

/// Decodifica um quadro: imagem inteira (se pedida) e miniatura.
pub fn decode(
    frame: &BrowserFrame,
    full: bool,
) -> Option<(Option<ColorImage>, ColorImage, [usize; 2])> {
    let format = match frame.format {
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImageFormat::Png => image::ImageFormat::Png,
    };
    let img = match image::load_from_memory_with_format(&frame.data, format) {
        Ok(img) => img.to_rgb8(),
        Err(err) => {
            warn!(%err, "quadro do navegador inválido");
            return None;
        }
    };
    let (w, h) = img.dimensions();
    let size = [w as usize, h as usize];
    let thumb_h = (h * THUMB_WIDTH / w.max(1)).max(1);
    let thumb = image::imageops::thumbnail(&img, THUMB_WIDTH.min(w), thumb_h.min(h));
    let thumb = ColorImage::from_rgb(
        [thumb.width() as usize, thumb.height() as usize],
        thumb.as_raw(),
    );
    let full = full.then(|| ColorImage::from_rgb(size, img.as_raw()));
    Some((full, thumb, size))
}

/// Task que drena o canal de quadros. Quadros de quem não está na tela são
/// descartados sem decodificar; atrasos pulam para o mais novo.
pub async fn run_frame_decoder(
    mut rx: EventReceiver,
    store: Arc<FrameStore>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    loop {
        let (id, frame) = match rx.recv().await {
            Ok(AgentEvent::BrowserFrame { id, frame }) => (id, frame),
            Ok(_) => continue,
            Err(RecvError::Lagged(n)) => {
                debug!(n, "quadros do navegador pulados");
                continue;
            }
            Err(RecvError::Closed) => break,
        };
        let Some(full) = store.wants(id) else {
            continue;
        };
        if store.last_seq(id).is_some_and(|s| s >= frame.seq) {
            continue;
        }
        let f = frame.clone();
        let Ok(Some((image, thumb, size))) =
            tokio::task::spawn_blocking(move || decode(&f, full)).await
        else {
            continue;
        };
        store.put(
            id,
            Decoded {
                seq: frame.seq,
                frame,
                size,
                image,
                thumb: Some(thumb),
            },
        );
        repaint();
    }
}

/// Ponto da tela sobre a imagem → coordenadas CSS do viewport.
pub fn to_css(frame: &BrowserFrame, image_rect: Rect, pos: Pos2) -> Option<(f64, f64)> {
    if !image_rect.contains(pos) || image_rect.width() <= 0.0 || image_rect.height() <= 0.0 {
        return None;
    }
    let u = (pos.x - image_rect.left()) / image_rect.width();
    let v = (pos.y - image_rect.top()) / image_rect.height();
    Some(frame.to_viewport(u, v))
}

/// Maior retângulo com a proporção da imagem dentro de `area`, centralizado.
pub fn fit_rect(area: Rect, image_size: [usize; 2]) -> Rect {
    let (w, h) = (image_size[0] as f32, image_size[1] as f32);
    if w <= 0.0 || h <= 0.0 {
        return area;
    }
    let scale = (area.width() / w).min(area.height() / h);
    Rect::from_center_size(area.center(), egui::vec2(w * scale, h * scale))
}

pub fn modifiers(m: egui::Modifiers) -> Modifiers {
    Modifiers {
        alt: m.alt,
        ctrl: m.ctrl,
        shift: m.shift,
        meta: m.mac_cmd,
    }
}

/// `KeyboardEvent.key` e `.code` de uma tecla não imprimível (ou com Ctrl).
/// Teclas imprimíveis sem Ctrl vão como texto (`BrowserInput::Text`).
pub fn dom_key(key: Key, m: egui::Modifiers) -> Option<(String, String)> {
    let named = |k: &str, c: &str| Some((k.to_owned(), c.to_owned()));
    match key {
        Key::Enter => named("Enter", "Enter"),
        Key::Tab => named("Tab", "Tab"),
        Key::Backspace => named("Backspace", "Backspace"),
        Key::Escape => named("Escape", "Escape"),
        Key::Delete => named("Delete", "Delete"),
        Key::Insert => named("Insert", "Insert"),
        Key::Home => named("Home", "Home"),
        Key::End => named("End", "End"),
        Key::PageUp => named("PageUp", "PageUp"),
        Key::PageDown => named("PageDown", "PageDown"),
        Key::ArrowUp => named("ArrowUp", "ArrowUp"),
        Key::ArrowDown => named("ArrowDown", "ArrowDown"),
        Key::ArrowLeft => named("ArrowLeft", "ArrowLeft"),
        Key::ArrowRight => named("ArrowRight", "ArrowRight"),
        _ => {
            if !(m.ctrl || m.mac_cmd) {
                return None;
            }
            let name = key.name();
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphabetic() => {
                    let k = if m.shift {
                        c.to_ascii_uppercase()
                    } else {
                        c.to_ascii_lowercase()
                    };
                    Some((k.to_string(), format!("Key{}", c.to_ascii_uppercase())))
                }
                (Some(c), None) if c.is_ascii_digit() => Some((c.to_string(), format!("Digit{c}"))),
                _ => None,
            }
        }
    }
}

/// Par Down/Up de uma tecla DOM.
pub fn key_pair(key: String, code: String, text: Option<String>, m: Modifiers) -> [KeyInput; 2] {
    [
        KeyInput {
            action: KeyAction::Down,
            key: key.clone(),
            code: code.clone(),
            text,
            modifiers: m,
        },
        KeyInput {
            action: KeyAction::Up,
            key,
            code,
            text: None,
            modifiers: m,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2};
    use orquestra_core::bytes::Bytes;

    fn frame(w: f64, h: f64) -> BrowserFrame {
        BrowserFrame {
            seq: 1,
            format: ImageFormat::Jpeg,
            data: Bytes::new(),
            css_width: w,
            css_height: h,
            page_scale: 1.0,
            offset_top: 0.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
        }
    }

    #[test]
    fn screen_point_maps_to_css_viewport() {
        let f = frame(1280.0, 800.0);
        let rect = Rect::from_min_size(pos2(100.0, 50.0), vec2(640.0, 400.0));
        assert_eq!(to_css(&f, rect, pos2(100.0, 50.0)), Some((0.0, 0.0)));
        assert_eq!(to_css(&f, rect, pos2(420.0, 250.0)), Some((640.0, 400.0)));
        assert_eq!(to_css(&f, rect, pos2(740.0, 450.0)), Some((1280.0, 800.0)));
        assert_eq!(to_css(&f, rect, pos2(99.0, 60.0)), None);
    }

    #[test]
    fn fit_keeps_aspect_and_centers() {
        let area = Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 400.0));
        let r = fit_rect(area, [1280, 800]);
        assert_eq!(r.height(), 400.0);
        assert_eq!(r.width(), 640.0);
        assert_eq!(r.center(), area.center());
    }

    #[test]
    fn dom_keys() {
        let none = egui::Modifiers::NONE;
        let ctrl = egui::Modifiers {
            ctrl: true,
            command: true,
            ..none
        };
        assert_eq!(
            dom_key(Key::Enter, none),
            Some(("Enter".to_owned(), "Enter".to_owned()))
        );
        assert_eq!(
            dom_key(Key::ArrowLeft, none).unwrap().0,
            "ArrowLeft".to_owned()
        );
        assert_eq!(dom_key(Key::A, none), None);
        assert_eq!(
            dom_key(Key::A, ctrl),
            Some(("a".to_owned(), "KeyA".to_owned()))
        );
        assert_eq!(dom_key(Key::Num1, ctrl).unwrap().1, "Digit1");
        assert!(modifiers(ctrl).ctrl);
    }

    #[test]
    fn decode_makes_thumbnail() {
        let img = image::RgbImage::from_pixel(640, 400, image::Rgb([200, 10, 10]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80)
            .encode_image(&img)
            .unwrap();
        let mut f = frame(640.0, 400.0);
        f.data = Bytes::from(jpeg);
        let (full, thumb, size) = decode(&f, true).unwrap();
        assert_eq!(size, [640, 400]);
        assert_eq!(full.unwrap().size, [640, 400]);
        assert_eq!(thumb.size, [320, 200]);
        let (full, _, _) = decode(&f, false).unwrap();
        assert!(full.is_none());
    }

    #[test]
    fn store_drops_full_image_when_not_wanted() {
        let store = FrameStore::default();
        let id = AgentId::generate();
        store.set_wanted(HashMap::from([(id, true)]));
        store.put(
            id,
            Decoded {
                seq: 3,
                frame: Arc::new(frame(1.0, 1.0)),
                size: [1, 1],
                image: Some(ColorImage::filled([1, 1], egui::Color32::RED)),
                thumb: Some(ColorImage::filled([1, 1], egui::Color32::RED)),
            },
        );
        assert_eq!(store.wants(id), Some(true));
        store.set_wanted(HashMap::new());
        assert!(store.take_image(id).is_none());
        assert!(store.take_thumb(id).is_some());
        assert_eq!(store.last_seq(id), Some(3));
    }
}
