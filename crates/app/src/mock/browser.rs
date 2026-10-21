//! Navegador falso: uma "página" desenhada com `image` (login, produtos),
//! que reage a clique, texto e navegação e publica quadros JPEG só com o
//! streaming ligado — como o screencast real, só quando algo muda.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use image::{Rgb, RgbImage};
use orquestra_core::bytes::Bytes;
use orquestra_core::{
    AgentEvent, AgentId, BrowserEndpoint, BrowserFrame, BrowserInput, BrowserManager,
    BrowserStatus, Error, EventBus, ImageFormat, KeyAction, MouseAction, Navigation, Result,
    Viewport,
};

use super::font::{draw_text, text_width};
use crate::feed::lock;

const HOME: &str = "http://localhost:5173/login";

#[derive(Clone)]
struct Page {
    endpoint: BrowserEndpoint,
    history: Vec<String>,
    pos: usize,
    fields: [String; 2],
    focus: Option<usize>,
    last_click: Option<(f64, f64)>,
    viewport: Viewport,
    streaming: bool,
    seq: u64,
    message: Option<String>,
}

impl Page {
    fn url(&self) -> &str {
        &self.history[self.pos]
    }

    fn path(&self) -> &str {
        let url = self.url();
        url.find("://")
            .and_then(|i| url[i + 3..].find('/').map(|j| &url[i + 3 + j..]))
            .unwrap_or("/")
    }

    fn title(&self) -> String {
        match self.path() {
            "/login" | "/" => "Entrar · Loja".to_owned(),
            "/produtos" => "Produtos · Loja".to_owned(),
            p => format!("{p} · Loja"),
        }
    }

    fn size(&self) -> (u32, u32) {
        (
            self.viewport.width.clamp(200, 1920),
            self.viewport.height.clamp(150, 1200),
        )
    }

    /// Retângulos (x, y, w, h) dos campos e do botão no login.
    fn login_layout(&self) -> [(i32, i32, i32, i32); 3] {
        let (w, _) = self.size();
        let card_w = 420.min(w as i32 - 40);
        let x = (w as i32 - card_w) / 2 + 30;
        let fw = card_w - 60;
        [(x, 200, fw, 44), (x, 290, fw, 44), (x, 360, fw, 48)]
    }

    fn navigate(&mut self, url: String) {
        self.history.truncate(self.pos + 1);
        self.history.push(url);
        self.pos = self.history.len() - 1;
        self.focus = None;
        self.message = None;
    }

    fn click(&mut self, x: f64, y: f64) -> bool {
        self.last_click = Some((x, y));
        if !matches!(self.path(), "/login" | "/") {
            return false;
        }
        let hit = |(rx, ry, rw, rh): (i32, i32, i32, i32)| {
            x >= f64::from(rx)
                && x <= f64::from(rx + rw)
                && y >= f64::from(ry)
                && y <= f64::from(ry + rh)
        };
        let [email, senha, botao] = self.login_layout();
        self.focus = if hit(email) {
            Some(0)
        } else if hit(senha) {
            Some(1)
        } else {
            None
        };
        if hit(botao) {
            return self.submit();
        }
        false
    }

    /// `true` se navegou.
    fn submit(&mut self) -> bool {
        if self.fields[0].contains('@') && !self.fields[1].is_empty() {
            let base = self.url().split("/login").next().unwrap_or(HOME).to_owned();
            self.navigate(format!("{base}/produtos"));
            true
        } else {
            self.message = Some("Informe e-mail e senha".to_owned());
            false
        }
    }

    fn render(&self) -> Vec<u8> {
        let (w, h) = self.size();
        let mut img = RgbImage::from_pixel(w, h, Rgb([0xf4, 0xf5, 0xf7]));
        fill(&mut img, 0, 0, w as i32, 64, [0x1f, 0x29, 0x37]);
        draw_text(&mut img, 24, 22, "LOJA", 3, [0xff, 0xff, 0xff]);
        let nav = "PRODUTOS   CARRINHO   ENTRAR";
        let nx = w as i32 - text_width(nav, 2) as i32 - 24;
        if nx > 24 + text_width("LOJA", 3) as i32 + 24 {
            draw_text(&mut img, nx, 26, nav, 2, [0xc8, 0xd0, 0xdc]);
        }

        match self.path() {
            "/login" | "/" => self.render_login(&mut img),
            "/produtos" => render_products(&mut img),
            other => {
                draw_text(
                    &mut img,
                    40,
                    120,
                    &format!("PAGINA {other}"),
                    3,
                    [0x33, 0x33, 0x33],
                );
            }
        }

        // Rodapé com a URL.
        fill(&mut img, 0, h as i32 - 28, w as i32, 28, [0xe2, 0xe5, 0xea]);
        draw_text(
            &mut img,
            12,
            h as i32 - 20,
            self.url(),
            2,
            [0x55, 0x5f, 0x6d],
        );

        if let Some((x, y)) = self.last_click {
            ring(&mut img, x as i32, y as i32, 12, [0xe5, 0x3e, 0x3e]);
            ring(&mut img, x as i32, y as i32, 11, [0xe5, 0x3e, 0x3e]);
        }
        let mut jpeg = Vec::new();
        let _ =
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 70).encode_image(&img);
        jpeg
    }

    fn render_login(&self, img: &mut RgbImage) {
        let [email, senha, botao] = self.login_layout();
        let card_x = email.0 - 30;
        fill(img, card_x, 110, email.2 + 60, 340, [0xff, 0xff, 0xff]);
        draw_text(img, email.0, 130, "ENTRAR NA LOJA", 3, [0x11, 0x18, 0x27]);
        draw_text(img, email.0, 180, "E-MAIL", 2, [0x55, 0x5f, 0x6d]);
        draw_text(img, email.0, 270, "SENHA", 2, [0x55, 0x5f, 0x6d]);
        for (i, r) in [email, senha].iter().enumerate() {
            let border = if self.focus == Some(i) {
                [0x25, 0x63, 0xeb]
            } else {
                [0xc9, 0xcf, 0xd8]
            };
            fill(img, r.0, r.1, r.2, r.3, border);
            fill(img, r.0 + 2, r.1 + 2, r.2 - 4, r.3 - 4, [0xff, 0xff, 0xff]);
            let shown = if i == 1 {
                "*".repeat(self.fields[1].chars().count())
            } else {
                self.fields[0].clone()
            };
            let caret = if self.focus == Some(i) { "_" } else { "" };
            draw_text(
                img,
                r.0 + 12,
                r.1 + 15,
                &format!("{shown}{caret}"),
                2,
                [0x11, 0x18, 0x27],
            );
        }
        fill(img, botao.0, botao.1, botao.2, botao.3, [0x25, 0x63, 0xeb]);
        let label = "ENTRAR";
        let lx = botao.0 + (botao.2 - text_width(label, 2) as i32) / 2;
        draw_text(img, lx, botao.1 + 17, label, 2, [0xff, 0xff, 0xff]);
        if let Some(msg) = &self.message {
            draw_text(img, email.0, 422, msg, 2, [0xdc, 0x26, 0x26]);
        }
    }
}

fn render_products(img: &mut RgbImage) {
    draw_text(img, 40, 96, "PRODUTOS", 3, [0x11, 0x18, 0x27]);
    let (w, _) = img.dimensions();
    let cols = ((w as i32 - 40) / 260).clamp(1, 4);
    let names = [
        "CAMISETA", "CANECA", "BONE", "MOCHILA", "CADERNO", "ADESIVOS",
    ];
    for (i, name) in names.iter().enumerate() {
        let (c, r) = (i as i32 % cols, i as i32 / cols);
        let x = 40 + c * 260;
        let y = 140 + r * 250;
        fill(img, x, y, 236, 226, [0xff, 0xff, 0xff]);
        fill(img, x + 12, y + 12, 212, 130, [0xdd, 0xe3, 0xec]);
        draw_text(img, x + 12, y + 156, name, 2, [0x11, 0x18, 0x27]);
        let price = format!("R$ {},90", 29 + i * 10);
        draw_text(img, x + 12, y + 186, &price, 2, [0x16, 0xa3, 0x4a]);
    }
}

fn fill(img: &mut RgbImage, x: i32, y: i32, w: i32, h: i32, color: [u8; 3]) {
    let (iw, ih) = (img.width() as i32, img.height() as i32);
    for py in y.max(0)..(y + h).min(ih) {
        for px in x.max(0)..(x + w).min(iw) {
            img.put_pixel(px as u32, py as u32, Rgb(color));
        }
    }
}

fn ring(img: &mut RgbImage, cx: i32, cy: i32, r: i32, color: [u8; 3]) {
    let (iw, ih) = (img.width() as i32, img.height() as i32);
    for i in 0..360 {
        let a = f64::from(i).to_radians();
        let x = cx + (f64::from(r) * a.cos()).round() as i32;
        let y = cy + (f64::from(r) * a.sin()).round() as i32;
        if (0..iw).contains(&x) && (0..ih).contains(&y) {
            img.put_pixel(x as u32, y as u32, Rgb(color));
        }
    }
}

pub struct MockBrowser {
    bus: EventBus,
    pages: Mutex<HashMap<AgentId, Page>>,
    next_port: AtomicU16,
    /// Multiplica os atrasos simulados (testes usam valores pequenos).
    scale: f64,
}

impl MockBrowser {
    pub fn new(bus: EventBus, scale: f64) -> Self {
        Self {
            bus,
            pages: Mutex::new(HashMap::new()),
            next_port: AtomicU16::new(41745),
            scale,
        }
    }

    fn with_page<T>(&self, id: AgentId, f: impl FnOnce(&mut Page) -> T) -> Result<T> {
        let mut pages = lock(&self.pages);
        let page = pages
            .get_mut(&id)
            .ok_or_else(|| Error::Browser("navegador não está aberto".to_owned()))?;
        Ok(f(page))
    }

    fn navigated(&self, id: AgentId, page: &Page) {
        self.bus.publish(AgentEvent::BrowserNavigated {
            id,
            url: page.url().to_owned(),
            title: page.title(),
        });
    }

    /// Publica um quadro, se o streaming estiver ligado.
    async fn frame(&self, id: AgentId) {
        let page = {
            let mut pages = lock(&self.pages);
            let Some(page) = pages.get_mut(&id) else {
                return;
            };
            if !page.streaming {
                return;
            }
            page.seq += 1;
            page.clone()
        };
        let Ok(jpeg) = tokio::task::spawn_blocking({
            let page = page.clone();
            move || page.render()
        })
        .await
        else {
            return;
        };
        let (w, h) = page.size();
        self.bus.publish(AgentEvent::BrowserFrame {
            id,
            frame: Arc::new(BrowserFrame {
                seq: page.seq,
                format: ImageFormat::Jpeg,
                data: Bytes::from(jpeg),
                css_width: f64::from(w),
                css_height: f64::from(h),
                page_scale: 1.0,
                offset_top: 0.0,
                scroll_x: 0.0,
                scroll_y: 0.0,
            }),
        });
    }
}

impl BrowserManager for MockBrowser {
    async fn launch(&self, id: AgentId) -> Result<BrowserEndpoint> {
        if let Some(page) = lock(&self.pages).get(&id) {
            return Ok(page.endpoint.clone());
        }
        self.bus.publish(AgentEvent::BrowserStatus {
            id,
            status: BrowserStatus::Launching,
        });
        tokio::time::sleep(Duration::from_secs_f64(0.4 * self.scale)).await;
        let port = self.next_port.fetch_add(1, Ordering::Relaxed);
        let endpoint = BrowserEndpoint {
            port,
            ws_url: format!("ws://127.0.0.1:{port}/devtools/browser/mock-{}", id.short()),
        };
        let page = Page {
            endpoint: endpoint.clone(),
            history: vec![HOME.to_owned()],
            pos: 0,
            fields: Default::default(),
            focus: None,
            last_click: None,
            viewport: Viewport::default(),
            streaming: false,
            seq: 0,
            message: None,
        };
        self.navigated(id, &page);
        lock(&self.pages).insert(id, page);
        self.bus.publish(AgentEvent::BrowserStatus {
            id,
            status: BrowserStatus::Ready {
                endpoint: endpoint.clone(),
            },
        });
        Ok(endpoint)
    }

    async fn close(&self, id: AgentId) -> Result<()> {
        if lock(&self.pages).remove(&id).is_some() {
            self.bus.publish(AgentEvent::BrowserStatus {
                id,
                status: BrowserStatus::Closed,
            });
        }
        Ok(())
    }

    async fn set_streaming(&self, id: AgentId, on: bool) -> Result<()> {
        self.with_page(id, |p| p.streaming = on)?;
        if on {
            self.frame(id).await;
        }
        Ok(())
    }

    async fn navigate(&self, id: AgentId, nav: Navigation) -> Result<()> {
        let page = self.with_page(id, |p| {
            match nav {
                Navigation::Url(url) => {
                    let url = if url.contains("://") {
                        url
                    } else {
                        format!("http://{url}")
                    };
                    p.navigate(url);
                }
                Navigation::Back => p.pos = p.pos.saturating_sub(1),
                Navigation::Forward => p.pos = (p.pos + 1).min(p.history.len() - 1),
                Navigation::Reload => {}
            }
            p.clone()
        })?;
        self.navigated(id, &page);
        self.frame(id).await;
        Ok(())
    }

    async fn input(&self, id: AgentId, input: BrowserInput) -> Result<()> {
        let (changed, navigated) = self.with_page(id, |p| match input {
            BrowserInput::Mouse(m) if m.action == MouseAction::Down => {
                let nav = p.click(m.x, m.y);
                (true, nav.then(|| p.clone()))
            }
            BrowserInput::Mouse(_) | BrowserInput::Wheel(_) => (false, None),
            BrowserInput::Text(text) => match p.focus {
                Some(i) => {
                    p.fields[i].push_str(&text);
                    (true, None)
                }
                None => (false, None),
            },
            BrowserInput::Key(key) if key.action == KeyAction::Down => match key.key.as_str() {
                "Backspace" => {
                    if let Some(i) = p.focus {
                        p.fields[i].pop();
                    }
                    (true, None)
                }
                "Tab" => {
                    p.focus = Some(p.focus.map_or(0, |f| (f + 1) % 2));
                    (true, None)
                }
                "Enter" => {
                    let nav = p.submit();
                    (true, nav.then(|| p.clone()))
                }
                _ => (false, None),
            },
            BrowserInput::Key(_) => (false, None),
        })?;
        if let Some(page) = navigated {
            self.navigated(id, &page);
        }
        if changed {
            self.frame(id).await;
        }
        Ok(())
    }

    async fn resize(&self, id: AgentId, viewport: Viewport) -> Result<()> {
        self.with_page(id, |p| p.viewport = viewport)?;
        self.frame(id).await;
        Ok(())
    }

    async fn shutdown(&self) {
        lock(&self.pages).clear();
    }
}

#[cfg(test)]
mod tests {
    use orquestra_core::{MouseButton, MouseInput};

    use super::*;

    #[tokio::test]
    async fn login_flow_navigates_and_streams_frames() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let mut frames = bus.subscribe_frames();
        let b = MockBrowser::new(bus.clone(), 0.0);
        let id = AgentId::generate();
        let ep = b.launch(id).await.unwrap();
        assert_eq!(b.launch(id).await.unwrap(), ep);

        b.set_streaming(id, true).await.unwrap();
        let AgentEvent::BrowserFrame { frame, .. } = frames.recv().await.unwrap() else {
            panic!("esperava quadro");
        };
        assert_eq!(frame.css_width, 1280.0);
        assert!(image::load_from_memory(&frame.data).is_ok());

        let click = |x, y| {
            BrowserInput::Mouse(MouseInput {
                action: MouseAction::Down,
                x,
                y,
                button: MouseButton::Left,
                click_count: 1,
                modifiers: Default::default(),
            })
        };
        let [email, senha, botao] = lock(&b.pages)[&id].login_layout();
        b.input(id, click(f64::from(email.0 + 5), f64::from(email.1 + 5)))
            .await
            .unwrap();
        b.input(id, BrowserInput::Text("eu@loja.com".to_owned()))
            .await
            .unwrap();
        b.input(id, click(f64::from(senha.0 + 5), f64::from(senha.1 + 5)))
            .await
            .unwrap();
        b.input(id, BrowserInput::Text("123".to_owned()))
            .await
            .unwrap();
        b.input(id, click(f64::from(botao.0 + 5), f64::from(botao.1 + 5)))
            .await
            .unwrap();
        let mut url = String::new();
        while let Ok(ev) = events.try_recv() {
            if let AgentEvent::BrowserNavigated { url: u, .. } = ev {
                url = u;
            }
        }
        assert!(url.ends_with("/produtos"), "{url}");

        b.set_streaming(id, false).await.unwrap();
        b.close(id).await.unwrap();
        assert!(b.set_streaming(id, true).await.is_err());
    }
}
