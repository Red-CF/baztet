//! The walking cat: 8-frame side-view cycle in three sizes and two
//! directions, embedded at build time and parsed once at first use.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Embedded sprite sheet: `@@ <tamaño> <dirección> <frame>` headers with
/// fixed-width body lines. The art is sacred: parse, never reformat.
pub const SHEET: &str = include_str!("sprites/gato_caminando.txt");

/// Sprite sizes, wide to narrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tamano {
    Grande,  // 32x10
    Mediano, // 24x8
    Pequeno, // 20x6
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direccion {
    Derecha,
    Izquierda,
}

impl Tamano {
    /// Grid size of this sprite: (width, height).
    pub fn dims(self) -> (u16, u16) {
        match self {
            Tamano::Grande => (32, 10),
            Tamano::Mediano => (24, 8),
            Tamano::Pequeno => (20, 6),
        }
    }

    /// Ordered wide to narrow for automatic selection.
    pub const ALL: [Tamano; 3] = [Tamano::Grande, Tamano::Mediano, Tamano::Pequeno];
}

pub type Frame = Vec<String>;

fn parse_sheet() -> HashMap<(Tamano, Direccion), Vec<Frame>> {
    let mut map: HashMap<(Tamano, Direccion), Vec<Frame>> = HashMap::new();
    let mut current: Option<((Tamano, Direccion), Frame)> = None;
    let finish = |current: &mut Option<((Tamano, Direccion), Frame)>,
                  map: &mut HashMap<(Tamano, Direccion), Vec<Frame>>| {
        if let Some((key, frame)) = current.take() {
            map.entry(key).or_default().push(frame);
        }
    };

    for line in SHEET.lines() {
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            finish(&mut current, &mut map);
            let mut parts = rest.split_whitespace();
            let (w, h) = parts
                .next()
                .and_then(|s| {
                    s.split_once('x')
                        .map(|(w, h)| (w.parse::<u16>().ok(), h.parse::<u16>().ok()))
                })
                .unwrap_or((None, None));
            let size = match (w, h) {
                (Some(32), Some(10)) => Some(Tamano::Grande),
                (Some(24), Some(8)) => Some(Tamano::Mediano),
                (Some(20), Some(6)) => Some(Tamano::Pequeno),
                _ => None,
            };
            let dir = match parts.next() {
                Some("derecha") => Some(Direccion::Derecha),
                Some("izquierda") => Some(Direccion::Izquierda),
                _ => None,
            };
            let _frame_no = parts.next();
            if let (Some(size), Some(dir)) = (size, dir) {
                current = Some(((size, dir), Frame::new()));
            }
            continue;
        }
        if let Some((_, frame)) = current.as_mut() {
            frame.push(line.to_string());
        }
    }
    finish(&mut current, &mut map);
    map
}

/// All parsed frames, parsed once. The loader does NOT pad: the sheet's
/// lines arrive fixed-width already; the acceptance test below guards it.
pub fn sheet() -> &'static HashMap<(Tamano, Direccion), Vec<Frame>> {
    static SHEETS: OnceLock<HashMap<(Tamano, Direccion), Vec<Frame>>> = OnceLock::new();
    SHEETS.get_or_init(parse_sheet)
}

/// The 8-frame cycle for one size+direction set.
pub fn frames(size: Tamano, dir: Direccion) -> &'static [Frame] {
    sheet().get(&(size, dir)).map(Vec::as_slice).unwrap_or(&[])
}

impl Direccion {
    fn step(self) -> i32 {
        match self {
            Direccion::Derecha => 1,
            Direccion::Izquierda => -1,
        }
    }
}

/// One cat walking across the terminal bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GatoPaseando {
    pub tamano: Tamano,
    pub dir: Direccion,
    /// sprite column (i32 so it can walk in from a negative lead)
    pub pos: i32,
    pub frame: usize,
    pub ancho_terminal: u16,
    pub alto_terminal: u16,
}

impl GatoPaseando {
    /// Pick the largest sprite that fits: the sheet's smallest is the
    /// floor; below that, the caller omits the animation.
    pub fn new(ancho_terminal: u16, alto_terminal: u16) -> Option<Self> {
        let tamano = *Tamano::ALL.iter().find(|t| {
            let (w, h) = t.dims();
            ancho_terminal >= w + 4 && alto_terminal >= h + 5
        })?;
        Some(Self {
            tamano,
            dir: Direccion::Derecha,
            pos: 1,
            frame: 0,
            ancho_terminal,
            alto_terminal,
        })
    }

    pub fn frame_lines(&self) -> &'static Frame {
        frames(self.tamano, self.dir)
            .get(self.frame % 8)
            .unwrap_or(&EMPTY)
    }

    /// Advance one frame and one column; at the edges, turn around.
    pub fn tick(&mut self) {
        self.frame = (self.frame + 1) % 8;
        self.pos += self.dir.step();
        let (w, _) = self.tamano.dims();
        if self.dir == Direccion::Derecha && self.pos + w as i32 > self.ancho_terminal as i32 {
            // the sprite's right edge left the grid: clamp and face back
            self.dir = Direccion::Izquierda;
            self.pos = self.ancho_terminal as i32 - w as i32;
        } else if self.dir == Direccion::Izquierda && self.pos < 0 {
            self.dir = Direccion::Derecha;
            self.pos = 0;
        }
    }
}

static EMPTY: Frame = Frame::new();

#[cfg(test)]
mod tests {
    use super::frames;
    use super::GatoPaseando;
    use super::Tamano;
    use super::{sheet, Direccion};
    use std::collections::HashSet;

    #[test]
    fn every_set_has_8_frames_with_identical_dims() {
        for tamano in Tamano::ALL {
            for dir in [Direccion::Derecha, Direccion::Izquierda] {
                let cycle = frames(tamano, dir);
                assert_eq!(cycle.len(), 8, "{tamano:?} {dir:?} cycle length");
                let (w, h) = tamano.dims();
                for (i, frame) in cycle.iter().enumerate() {
                    assert_eq!(
                        frame.len(),
                        h as usize,
                        "frame {i} height {tamano:?} {dir:?}"
                    );
                    for line in frame {
                        assert_eq!(line.len(), w as usize, "frame {i} width {tamano:?} {dir:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn sheet_covers_every_expected_set() {
        let mut seen: HashSet<(Tamano, Direccion)> = HashSet::new();
        seen.extend(sheet().keys().copied());
        assert_eq!(seen.len(), 6, "3 sizes x 2 directions");
    }

    #[test]
    fn walk_turns_at_the_edges_without_escape() {
        // a narrow terminal forces the small sprite; ticking from edge to
        // edge flips the direction and never walks outside the bounds
        // 30x20 fits the mediano sprite (grande needs 36)
        let mut gato = GatoPaseando::new(30, 20).unwrap();
        assert_eq!(gato.tamano, Tamano::Mediano);
        let mut dir_seen: HashSet<Direccion> = HashSet::new();
        for _ in 0..200 {
            let (w, _) = gato.tamano.dims();
            assert!(gato.pos >= 0 && gato.pos + w as i32 <= gato.ancho_terminal as i32);
            dir_seen.insert(gato.dir);
            gato.tick();
        }
        assert!(dir_seen.contains(&Direccion::Derecha));
        assert!(dir_seen.contains(&Direccion::Izquierda));
    }
}
