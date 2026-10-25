//! The walking cat: 8-frame side-view cycle in three sizes and two
//! directions, embedded at build time and parsed once at first use.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Embedded sprite sheet: `@@ <tamaño> <dirección> <frame>` headers with
/// fixed-width body lines. The art is sacred: parse, never reformat.
pub const SHEET: &str = include_str!("sprites/gato_silueta.txt");

/// Sprite sizes, wide to narrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpriteSize {
    Large,  // 32x12
    Medium, // 24x9
    Small,  // 18x7
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Right,
    Left,
}

impl SpriteSize {
    /// Grid size of this sprite: (width, height).
    pub fn dims(self) -> (u16, u16) {
        match self {
            SpriteSize::Large => (32, 12),
            SpriteSize::Medium => (24, 9),
            SpriteSize::Small => (18, 7),
        }
    }

    /// Ordered wide to narrow for automatic selection.
    pub const ALL: [SpriteSize; 3] = [SpriteSize::Large, SpriteSize::Medium, SpriteSize::Small];
}

pub type Frame = Vec<String>;

fn parse_sheet() -> HashMap<(SpriteSize, Direction), Vec<Frame>> {
    let mut map: HashMap<(SpriteSize, Direction), Vec<Frame>> = HashMap::new();
    let mut current: Option<((SpriteSize, Direction), Frame)> = None;
    let finish = |current: &mut Option<((SpriteSize, Direction), Frame)>,
                  map: &mut HashMap<(SpriteSize, Direction), Vec<Frame>>| {
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
                (Some(32), Some(12)) => Some(SpriteSize::Large),
                (Some(24), Some(9)) => Some(SpriteSize::Medium),
                (Some(18), Some(7)) => Some(SpriteSize::Small),
                _ => None,
            };
            let dir = match parts.next() {
                Some("derecha") => Some(Direction::Right),
                Some("izquierda") => Some(Direction::Left),
                _ => None,
            };
            let _frame_no = parts.next();
            if let (Some(size), Some(dir)) = (size, dir) {
                current = Some(((size, dir), Frame::new()));
            }
            continue;
        }
        if let Some(((size, _), frame)) = current.as_mut() {
            // the sheet declares a fixed width: pad short lines up (the
            // art's fixed-width rule holds; no right-trim ever)
            frame.push(format!("{:<width$}", line, width = size.dims().0 as usize));
        }
    }
    finish(&mut current, &mut map);
    map
}

/// All parsed frames, parsed once. The loader pads short body lines UP to
/// the sheet's declared width; the acceptance test guards the set's dims.
pub fn sheet() -> &'static HashMap<(SpriteSize, Direction), Vec<Frame>> {
    static SHEETS: OnceLock<HashMap<(SpriteSize, Direction), Vec<Frame>>> = OnceLock::new();
    SHEETS.get_or_init(parse_sheet)
}

/// The 8-frame cycle for one size+direction set.
pub fn frames(size: SpriteSize, dir: Direction) -> &'static [Frame] {
    sheet().get(&(size, dir)).map(Vec::as_slice).unwrap_or(&[])
}

impl Direction {
    fn step(self) -> i32 {
        match self {
            Direction::Right => 1,
            Direction::Left => -1,
        }
    }
}

/// One cat walking across the terminal bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkingCat {
    pub size: SpriteSize,
    pub dir: Direction,
    /// sprite column (i32 so it can walk in from a negative lead)
    pub pos: i32,
    pub frame: usize,
    pub terminal_width: u16,
    pub terminal_height: u16,
}

impl WalkingCat {
    /// Pick the largest sprite that fits: the sheet's smallest is the
    /// floor; below that, the caller omits the animation.
    pub fn new(terminal_width: u16, terminal_height: u16) -> Option<Self> {
        let size = *SpriteSize::ALL.iter().find(|t| {
            let (w, h) = t.dims();
            terminal_width >= w + 4 && terminal_height >= h + 5
        })?;
        Some(Self {
            size,
            dir: Direction::Right,
            pos: 1,
            frame: 0,
            terminal_width,
            terminal_height,
        })
    }

    pub fn frame_lines(&self) -> &'static Frame {
        frames(self.size, self.dir)
            .get(self.frame % 8)
            .unwrap_or(&EMPTY)
    }

    /// Advance one frame and one column; at the edges, turn around.
    pub fn tick(&mut self) {
        self.frame = (self.frame + 1) % 8;
        self.pos += self.dir.step();
        let (w, _) = self.size.dims();
        if self.dir == Direction::Right && self.pos + w as i32 > self.terminal_width as i32 {
            // the sprite's right edge left the grid: clamp and face back
            self.dir = Direction::Left;
            self.pos = self.terminal_width as i32 - w as i32;
        } else if self.dir == Direction::Left && self.pos < 0 {
            self.dir = Direction::Right;
            self.pos = 0;
        }
    }
}

static EMPTY: Frame = Frame::new();

#[cfg(test)]
mod tests {
    use super::frames;
    use super::{sheet, Direction, SpriteSize, WalkingCat};
    use std::collections::HashSet;

    #[test]
    fn every_set_has_8_frames_with_identical_dims() {
        for size in SpriteSize::ALL {
            for dir in [Direction::Right, Direction::Left] {
                let cycle = frames(size, dir);
                assert_eq!(cycle.len(), 8, "{size:?} {dir:?} cycle length");
                let (w, h) = size.dims();
                for (i, frame) in cycle.iter().enumerate() {
                    assert_eq!(frame.len(), h as usize, "frame {i} height {size:?} {dir:?}");
                    for line in frame {
                        assert_eq!(line.len(), w as usize, "frame {i} width {size:?} {dir:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn sheet_covers_every_expected_set() {
        let mut seen: HashSet<(SpriteSize, Direction)> = HashSet::new();
        seen.extend(sheet().keys().copied());
        assert_eq!(seen.len(), 6, "3 sizes x 2 directions");
    }

    #[test]
    fn walk_turns_at_the_edges_without_escape() {
        // a narrow terminal forces the small sprite; ticking from edge to
        // edge flips the direction and never walks outside the bounds
        // 30x20 fits the medium sprite (35-wide gate excludes Large)
        let mut walker = WalkingCat::new(30, 20).unwrap();
        assert_eq!(walker.size, SpriteSize::Medium);
        let mut dir_seen: HashSet<Direction> = HashSet::new();
        for _ in 0..200 {
            let (w, _) = walker.size.dims();
            assert!(walker.pos >= 0 && walker.pos + w as i32 <= walker.terminal_width as i32);
            dir_seen.insert(walker.dir);
            walker.tick();
        }
        assert!(dir_seen.contains(&Direction::Right));
        assert!(dir_seen.contains(&Direction::Left));
    }
}
