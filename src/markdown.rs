//! Markdown analysis for live preview.
//!
//! [`Document`] holds one [`LineInfo`] per source line: the line's block kind
//! ([`Leaf`]), its containers (quote depth, list item), and the line's
//! **rendered** spans. Every span records the source byte range it came from,
//! so the layout can map screen columns back to source columns. The raw view
//! of a line (cursor line, or a whole code block / table / front matter when
//! the cursor is inside) is derived from the same spans by [`raw_spans`]
//! without re-parsing.
//!
//! Parsing uses pulldown-cmark's offset iterator. [`Document::update`]
//! re-parses only the region around an edit: it restarts at a top-level block
//! boundary (or top-level list item) before the edit and stops at the first
//! such boundary after it that the new parse confirms, so typing in a 5 MB
//! note parses a few blocks. Edits inside front matter or inside a top-level
//! fenced code block re-parse just that line range, and link reference
//! definitions elsewhere in the note are handed to the partial parse so links
//! still resolve.

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::buffer::Damage;
use crate::layout::str_width;
use crate::store::{front_matter_lines, FrontMatter};

/// Inline emphasis on a span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Inline {
    pub strong: bool,
    pub emph: bool,
    pub strike: bool,
    pub link: bool,
    /// Alt text of an image.
    pub image: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    /// Prose (style comes from `Inline` and the line's `Leaf`).
    Text,
    /// Inline code or code-block content.
    Code,
    /// Markdown syntax shown in raw mode (`**`, `#`, `](url)`, fences...).
    Marker,
    /// `▎ ` for a `>`.
    QuoteBar,
    /// `• ` / `◦ ` / `▪ ` for a bullet marker.
    Bullet,
    /// `1. ` for an ordered marker.
    Number,
    /// `☐ ` / `☑ ` for `[ ]` / `[x]`.
    Checkbox { checked: bool },
    /// Indentation added for nesting (no source).
    Indent,
    /// Table borders and the delimiter row.
    TableSep,
    /// Padding that aligns table cells (no source).
    TablePad,
    /// Raw HTML.
    Html,
    /// The marker shown before an image's alt text (no source).
    ImageMarker,
    /// Front matter (collapsed summary when rendered).
    FrontMatter,
}

impl SpanKind {
    /// Decorations exist only on screen; clicking one maps to nearby content.
    pub fn is_decoration(self) -> bool {
        matches!(
            self,
            SpanKind::Indent | SpanKind::TablePad | SpanKind::ImageMarker | SpanKind::QuoteBar
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Byte range in the source line this span stands for. Empty for pure
    /// decorations (positioned where they appear).
    pub source: Range<usize>,
    /// What to show. `None` means the source text itself (verbatim), which
    /// makes column mapping inside the span exact.
    pub display: Option<String>,
    pub kind: SpanKind,
    pub inline: Inline,
}

impl Span {
    fn verbatim(source: Range<usize>, kind: SpanKind, inline: Inline) -> Span {
        Span {
            source,
            display: None,
            kind,
            inline,
        }
    }

    fn shown(source: Range<usize>, display: impl Into<String>, kind: SpanKind) -> Span {
        Span {
            source,
            display: Some(display.into()),
            kind,
            inline: Inline::default(),
        }
    }

    /// The text to draw. `line` is the source line the span belongs to.
    pub fn text<'a>(&'a self, line: &'a str) -> &'a str {
        match &self.display {
            Some(text) => text,
            None => line.get(self.source.clone()).unwrap_or(""),
        }
    }

    pub fn is_verbatim(&self) -> bool {
        self.display.is_none()
    }
}

/// The block a line belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Leaf {
    Blank,
    Paragraph,
    Heading {
        level: u8,
    },
    /// The `===` / `---` line under a setext heading (drawn as a rule).
    SetextUnderline {
        level: u8,
    },
    /// A ```` ``` ```` line; the opening one carries the language label.
    CodeFence {
        lang: Option<String>,
        opening: bool,
    },
    Code {
        fenced: bool,
    },
    TableRow {
        header: bool,
    },
    /// The `|---|:---:|` line (drawn as a border).
    TableDelimiter,
    /// A thematic break (drawn full width).
    Rule,
    Html,
    FrontMatter {
        first: bool,
    },
    /// Source the parser emitted nothing for (e.g. link reference
    /// definitions); shown dimmed and verbatim.
    Other,
}

/// A list item starting on this line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemInfo {
    /// 1 for a top-level item.
    pub depth: u8,
    /// The number for an ordered item.
    pub number: Option<u64>,
    /// `Some(checked)` for a task item.
    pub task: Option<bool>,
    /// The marker and the spaces after it.
    pub marker: Range<usize>,
    /// The `[ ]` / `[x]` source range.
    pub checkbox: Option<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineInfo {
    pub leaf: Leaf,
    pub quote_depth: u8,
    /// How many list items contain this line.
    pub item_depth: u8,
    /// Set when a list item starts on this line.
    pub item: Option<ItemInfo>,
    /// The task state of the innermost list item containing this line
    /// (`Some(true)` on every line of a checked task, not just its first).
    pub task: Option<bool>,
    /// The rendered line, in display order.
    pub spans: Vec<Span>,
    /// Drawn at the start of soft-wrapped continuation rows (quote bars,
    /// list indentation) when rendered.
    pub cont: Vec<Span>,
    /// Takes no screen rows when rendered (collapsed front matter).
    pub hidden: bool,
    /// Lines shown raw together with this one: `index - group_back ..
    /// index - group_back + group_len` (0 length = just this line).
    group_back: u32,
    group_len: u32,
    /// A top-level block starts at column 0 of this line.
    block_start: bool,
    /// The parser emitted nothing for this line's text (a link reference
    /// definition, possibly one line of a longer one): where that text starts,
    /// after any quote or list markers.
    ref_def: Option<usize>,
}

impl LineInfo {
    fn blank() -> LineInfo {
        LineInfo {
            leaf: Leaf::Blank,
            quote_depth: 0,
            item_depth: 0,
            item: None,
            task: None,
            spans: Vec::new(),
            cont: Vec::new(),
            hidden: false,
            group_back: 0,
            group_len: 0,
            block_start: false,
            ref_def: None,
        }
    }

    /// The lines shown raw with line `index` when the cursor is on it.
    pub fn raw_group(&self, index: usize) -> Range<usize> {
        if self.group_len == 0 {
            return index..index + 1;
        }
        let start = index.saturating_sub(self.group_back as usize);
        start..start + self.group_len as usize
    }

    /// A list item starting at column 0 of a top-level list: parsing can
    /// restart here, since every earlier block is closed.
    fn is_top_item(&self) -> bool {
        self.quote_depth == 0
            && self.item_depth == 1
            && self
                .item
                .as_ref()
                .is_some_and(|item| item.depth == 1 && item.marker.start == 0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontInfo {
    pub fence: &'static str,
    /// Index of the closing fence line (the opening one is line 0).
    pub end_line: usize,
    pub matter: FrontMatter,
}

/// The parsed note: one [`LineInfo`] per line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    lines: Vec<LineInfo>,
    front: Option<FrontInfo>,
    /// The note has link reference definitions: they resolve document-wide,
    /// so a partial parse is handed them (see `ref_prelude`).
    has_refs: bool,
}

impl Document {
    pub fn parse<S: AsRef<str>>(lines: &[S]) -> Document {
        let (front, mut infos) = match front_block(lines) {
            Some((front, infos)) => (Some(front), infos),
            None => (None, Vec::with_capacity(lines.len())),
        };
        let body_start = infos.len();
        infos.extend(parse_lines(lines, body_start, lines.len(), ""));
        let has_refs = infos.iter().any(|info| info.ref_def.is_some());
        Document {
            lines: infos,
            front,
            has_refs,
        }
    }

    /// Brings the document up to date after `damage` (from
    /// [`crate::buffer::Buffer::take_damage`]) turned the old text into
    /// `lines`. Returns the lines whose info changed, for layout caches.
    pub fn update<S: AsRef<str>>(&mut self, lines: &[S], damage: Damage) -> Damage {
        match self.try_incremental(lines, damage) {
            Some(changed) => changed,
            None => {
                let old_len = self.lines.len();
                *self = Document::parse(lines);
                Damage::full(old_len, lines.len())
            }
        }
    }

    fn try_incremental<S: AsRef<str>>(&mut self, lines: &[S], damage: Damage) -> Option<Damage> {
        let old_len = self.lines.len();
        let new_len = lines.len();
        if damage.start > damage.old_end
            || damage.start > damage.new_end
            || damage.old_end > old_len
            || damage.new_end > new_len
            || old_len as isize + damage.delta() != new_len as isize
        {
            return None;
        }
        let body_start = match &self.front {
            Some(front) => {
                if damage.start <= front.end_line {
                    return self.update_front_matter(lines, damage);
                }
                front.end_line + 1
            }
            None => {
                // The edit must not have turned the top into front matter.
                if front_matter_lines(lines).is_some() {
                    return None;
                }
                0
            }
        };
        // A definition added or removed re-resolves links everywhere.
        if lines[damage.start..damage.new_end]
            .iter()
            .any(|line| looks_like_ref_def(line.as_ref()))
            || self.lines[damage.start..damage.old_end]
                .iter()
                .any(|info| info.ref_def.is_some())
        {
            return None;
        }
        if let Some(changed) = self.update_code(lines, damage, body_start) {
            return Some(changed);
        }
        let map = |old: usize| old - damage.old_end + damage.new_end;
        let prelude = if self.has_refs {
            self.ref_prelude(lines, damage)
        } else {
            String::new()
        };
        // The restart line must itself be unchanged: an edited line can join
        // the block before it (e.g. a list across a blank line).
        let restart = (body_start..damage.start.min(old_len))
            .rev()
            .find(|&index| self.is_safe(index, body_start))
            .unwrap_or(body_start);
        let mut end = self.next_safe(damage.old_end.max(restart + 1), body_start);
        loop {
            if end >= old_len {
                let mut infos = parse_lines(lines, restart, new_len, &prelude);
                self.keep_restart_state(&mut infos, restart, body_start);
                if !self.same_ref_defs(restart..old_len, &infos, restart, damage) {
                    return None;
                }
                self.lines.splice(restart..old_len, infos);
                return Some(Damage {
                    start: restart,
                    old_end: old_len,
                    new_end: new_len,
                });
            }
            let window = self.next_safe(end + 1, body_start);
            let new_end = map(end);
            let window_new = if window >= old_len {
                new_len
            } else {
                map(window)
            };
            let mut infos = parse_lines(lines, restart, window_new, &prelude);
            self.keep_restart_state(&mut infos, restart, body_start);
            let at = new_end - restart;
            let old_info = &self.lines[end];
            // The old line at `end` parses exactly as before, and from there
            // on nothing depends on what came earlier: it starts a fresh
            // top-level block after a blank line, or it is the same top-level
            // list item (the list goes on as it did).
            let confirmed = at > 0
                && infos.get(at).is_some_and(|info| {
                    info == old_info
                        && ((info.block_start && infos[at - 1].leaf == Leaf::Blank)
                            || info.is_top_item())
                });
            if confirmed {
                infos.truncate(at);
                if !self.same_ref_defs(restart..end, &infos, restart, damage) {
                    return None;
                }
                self.lines.splice(restart..end, infos);
                return Some(Damage {
                    start: restart,
                    old_end: end,
                    new_end,
                });
            }
            // Grow geometrically so a newly opened fence costs O(n), not O(n^2).
            let target = restart + 2 * (window - restart).max(1);
            end = self.next_safe(target.max(window), body_start);
        }
    }

    /// A restart in the middle of a list parses its first item as the start
    /// of a new list; the line's block-start flag is the old parse's.
    fn keep_restart_state(&self, infos: &mut [LineInfo], restart: usize, body_start: usize) {
        if restart > body_start {
            if let (Some(first), Some(old)) = (infos.first_mut(), self.lines.get(restart)) {
                first.block_start = old.block_start;
            }
        }
    }

    /// Whether the re-parsed lines (new lines `restart..`) have link
    /// reference definitions on exactly the same, unedited lines as the old
    /// ones (`old`); else links elsewhere may resolve differently.
    fn same_ref_defs(
        &self,
        old: Range<usize>,
        infos: &[LineInfo],
        restart: usize,
        damage: Damage,
    ) -> bool {
        let map = |index: usize| {
            if index < damage.start {
                index
            } else {
                index - damage.old_end + damage.new_end
            }
        };
        let before = old
            .clone()
            .filter(|&index| self.lines[index].ref_def.is_some())
            .map(map);
        let after = infos
            .iter()
            .enumerate()
            .filter(|(_, info)| info.ref_def.is_some())
            .map(|(offset, _)| restart + offset);
        before.eq(after)
    }

    /// The note's link reference definitions outside the damaged lines, as a
    /// prelude for a partial parse so its links resolve as in a full one.
    /// Consecutive definition lines stay together (a definition can span
    /// lines); separate runs get a blank line between them. An HTML comment
    /// and a blank line end the prelude, so nothing in it (a stray list
    /// marker, say) can carry on into the parsed lines.
    fn ref_prelude<S: AsRef<str>>(&self, lines: &[S], damage: Damage) -> String {
        let mut prelude = String::new();
        let mut previous: Option<usize> = None;
        for (index, info) in self.lines.iter().enumerate() {
            let Some(at) = info.ref_def else {
                continue;
            };
            if (damage.start..damage.old_end).contains(&index) {
                continue;
            }
            let new_index = if index < damage.start {
                index
            } else {
                index - damage.old_end + damage.new_end
            };
            let Some(text) = lines
                .get(new_index)
                .and_then(|line| line.as_ref().get(at..))
            else {
                continue;
            };
            if previous.is_some_and(|previous| previous + 1 != index) {
                prelude.push('\n');
            }
            previous = Some(index);
            prelude.push_str(text);
            prelude.push('\n');
        }
        if !prelude.is_empty() {
            prelude.push_str("\n<!-- -->\n\n");
        }
        prelude
    }

    /// An edit inside the front matter that keeps both fences where they
    /// were: re-reads just the front matter.
    fn update_front_matter<S: AsRef<str>>(
        &mut self,
        lines: &[S],
        damage: Damage,
    ) -> Option<Damage> {
        let front = self.front.as_ref()?;
        if damage.start == 0 || damage.old_end > front.end_line {
            return None;
        }
        let new_end = (front.end_line as isize + damage.delta()) as usize;
        let (fresh, infos) = front_block(lines)?;
        if fresh.fence != front.fence || fresh.end_line != new_end {
            return None;
        }
        let old_end = front.end_line + 1;
        self.lines.splice(0..old_end, infos);
        self.front = Some(fresh);
        Some(Damage {
            start: 0,
            old_end,
            new_end: new_end + 1,
        })
    }

    /// An edit strictly inside a top-level fenced code block that adds no
    /// fence-like line: only the edited lines are re-parsed (code is literal),
    /// so typing in a huge code block stays cheap.
    fn update_code<S: AsRef<str>>(
        &mut self,
        lines: &[S],
        damage: Damage,
        body_start: usize,
    ) -> Option<Damage> {
        let anchor = damage
            .start
            .checked_sub(1)
            .filter(|&anchor| anchor >= body_start)?;
        let info = self.lines.get(anchor)?;
        let in_block = matches!(
            info.leaf,
            Leaf::CodeFence { opening: true, .. } | Leaf::Code { fenced: true }
        );
        if !in_block || info.quote_depth > 0 || info.item_depth > 0 || info.group_len == 0 {
            return None;
        }
        let group = info.raw_group(anchor);
        let open = group.start;
        let last = group.end.checked_sub(1)?;
        let closed = matches!(
            self.lines.get(last).map(|info| &info.leaf),
            Some(Leaf::CodeFence { opening: false, .. })
        );
        let old_len = self.lines.len();
        let touches_end = if closed {
            damage.old_end > last
        } else {
            group.end != old_len
        };
        if touches_end
            || lines[damage.start..damage.new_end]
                .iter()
                .any(|line| fence_like(line.as_ref()))
        {
            return None;
        }
        // Parse the edited lines between the block's own fences.
        let mut synth: Vec<&str> = Vec::with_capacity(damage.new_end - damage.start + 2);
        synth.push(lines.get(open)?.as_ref());
        synth.extend(
            lines[damage.start..damage.new_end]
                .iter()
                .map(AsRef::as_ref),
        );
        if closed {
            synth.push(lines.get(last - damage.old_end + damage.new_end)?.as_ref());
        }
        let parsed = parse_lines(&synth, 0, synth.len(), "");
        let count = damage.new_end - damage.start;
        if parsed.len() < count + 1
            || parsed[1..=count]
                .iter()
                .any(|info| info.leaf != Leaf::Code { fenced: true })
        {
            return None;
        }
        let fresh: Vec<LineInfo> = parsed.into_iter().skip(1).take(count).collect();
        self.lines.splice(damage.start..damage.old_end, fresh);
        let group_len = (group.end - open) as isize + damage.delta();
        let group_len = u32::try_from(group_len).ok()?;
        for index in open..open + group_len as usize {
            if let Some(info) = self.lines.get_mut(index) {
                info.group_back = (index - open) as u32;
                info.group_len = group_len;
            }
        }
        Some(damage)
    }

    fn is_safe(&self, index: usize, body_start: usize) -> bool {
        if index == body_start {
            return true;
        }
        if index < body_start {
            return false;
        }
        self.lines.get(index).is_some_and(|info| {
            (info.block_start && self.lines[index - 1].leaf == Leaf::Blank) || info.is_top_item()
        })
    }

    /// The first safe restart line at or after `from` (or the line count).
    fn next_safe(&self, from: usize, body_start: usize) -> usize {
        (from..self.lines.len())
            .find(|&index| self.is_safe(index, body_start))
            .unwrap_or(self.lines.len())
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn line(&self, index: usize) -> Option<&LineInfo> {
        self.lines.get(index)
    }

    pub fn lines(&self) -> &[LineInfo] {
        &self.lines
    }

    pub fn front_matter(&self) -> Option<&FrontInfo> {
        self.front.as_ref()
    }

    /// The lines shown raw when the cursor is on `line`.
    pub fn raw_range(&self, line: usize) -> Range<usize> {
        match self.lines.get(line) {
            Some(info) => info.raw_group(line),
            None => line..line + 1,
        }
    }

    /// Front matter title, else the first level-1 heading (ATX or setext)
    /// when it is the first non-blank body line, as rendered: markers and
    /// entities resolved.
    pub fn title<S: AsRef<str>>(&self, lines: &[S]) -> Option<String> {
        if let Some(title) = self
            .front
            .as_ref()
            .and_then(|front| front.matter.title.clone())
        {
            return Some(title);
        }
        let start = self.front.as_ref().map_or(0, |front| front.end_line + 1);
        let blank = |line: &str| {
            line.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
                .is_empty()
        };
        let index = (start..lines.len()).find(|&index| !blank(lines[index].as_ref()))?;
        let info = self.lines.get(index)?;
        if info.leaf != (Leaf::Heading { level: 1 }) || info.quote_depth > 0 || info.item_depth > 0
        {
            return None;
        }
        let line = lines[index].as_ref();
        let text: String = info
            .spans
            .iter()
            .filter(|span| matches!(span.kind, SpanKind::Text | SpanKind::Code))
            .map(|span| span.text(line))
            .collect();
        Some(crate::store::clean(text.trim())).filter(|title| !title.is_empty())
    }
}

/// The front matter's info and its lines' infos (the summary on line 0,
/// the rest hidden), if `lines` open with front matter.
fn front_block<S: AsRef<str>>(lines: &[S]) -> Option<(FrontInfo, Vec<LineInfo>)> {
    let (fence, end_line) = front_matter_lines(lines)?;
    let header: Vec<&str> = lines[1..end_line].iter().map(AsRef::as_ref).collect();
    let front = FrontInfo {
        fence,
        end_line,
        matter: FrontMatter::parse(&header.join("\n"), fence),
    };
    let len = end_line + 1;
    let mut infos = Vec::with_capacity(len);
    for index in 0..len {
        let mut info = LineInfo::blank();
        info.leaf = Leaf::FrontMatter { first: index == 0 };
        info.hidden = index > 0;
        info.group_back = index as u32;
        info.group_len = len as u32;
        if index == 0 {
            info.spans = vec![Span::shown(
                0..lines[0].as_ref().len(),
                front.matter.summary(),
                SpanKind::FrontMatter,
            )];
        }
        infos.push(info);
    }
    Some((front, infos))
}

/// A line that could open or close a fenced code block.
fn fence_like(line: &str) -> bool {
    let trimmed = line.trim_start_matches([' ', '\t']);
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

fn looks_like_ref_def(line: &str) -> bool {
    let trimmed = line.trim_start_matches([' ', '>']);
    trimmed.starts_with('[') && trimmed.contains("]:")
}

/// The raw view of a line: the whole source, verbatim, tinted by what each
/// part is (markers dimmed, code tinted, emphasis kept).
pub fn raw_spans(line: &str, info: &LineInfo) -> Vec<Span> {
    let gap_kind = match info.leaf {
        Leaf::Code { .. } => SpanKind::Code,
        Leaf::FrontMatter { .. } => SpanKind::FrontMatter,
        Leaf::Html => SpanKind::Html,
        _ => SpanKind::Marker,
    };
    let mut sources: Vec<&Span> = info
        .spans
        .iter()
        .filter(|span| !span.source.is_empty() && span.source.end <= line.len())
        .collect();
    sources.sort_by_key(|span| span.source.start);
    let mut out = Vec::new();
    let mut pos = 0;
    for span in sources {
        if span.source.start < pos {
            continue;
        }
        if span.source.start > pos {
            out.push(Span::verbatim(
                pos..span.source.start,
                gap_kind,
                Inline::default(),
            ));
        }
        let kind = match span.kind {
            SpanKind::TableSep => SpanKind::Marker,
            SpanKind::FrontMatter => SpanKind::FrontMatter,
            other => other,
        };
        out.push(Span::verbatim(span.source.clone(), kind, span.inline));
        pos = span.source.end;
    }
    if pos < line.len() {
        out.push(Span::verbatim(pos..line.len(), gap_kind, Inline::default()));
    }
    out
}

// ---------------------------------------------------------------------------
// Parsing a run of lines
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Container {
    Quote,
    Item {
        /// Marker range when the item starts on this line.
        marker: Option<Range<usize>>,
        depth: u8,
        number: Option<u64>,
        /// Display width of the marker (continuation indent).
        width: usize,
    },
}

#[derive(Debug, Default, Clone)]
struct LineBuild {
    leaf: Option<Leaf>,
    chain: Vec<Container>,
    item: Option<ItemInfo>,
    content: Vec<Span>,
    /// Table rows replace `content` with fully composed spans.
    composed: Option<Vec<Span>>,
    block_start: bool,
    group: Option<(usize, usize)>,
    /// Task state of the innermost list item containing the line, and that
    /// item's depth.
    task: Option<bool>,
    task_depth: u8,
}

struct TableBuild {
    aligns: Vec<Alignment>,
    first: usize,
    last: usize,
    rows: Vec<RowBuild>,
}

struct RowBuild {
    line: usize,
    header: bool,
    cells: Vec<Vec<Span>>,
    /// Source offset where the row's content ends (for decoration positions).
    end: usize,
}

const BULLETS: [&str; 3] = ["• ", "◦ ", "▪ "];

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

struct Builder<'a, S: AsRef<str>> {
    lines: &'a [S],
    first: usize,
    starts: Vec<usize>,
    text_len: usize,
    out: Vec<LineBuild>,
    container_depth: usize,
    item_depth: u8,
    lists: Vec<Option<u64>>,
    strong: u32,
    emph: u32,
    strike: u32,
    link: u32,
    image: u32,
    in_code: bool,
    table: Option<TableBuild>,
    cell: Option<Vec<Span>>,
    /// Open list items: local line range and depth.
    items: Vec<(usize, usize, u8)>,
    /// Open containers, innermost last: `true` for a list item.
    parents: Vec<bool>,
}

impl<'a, S: AsRef<str>> Builder<'a, S> {
    fn line_text(&self, local: usize) -> &'a str {
        self.lines
            .get(self.first + local)
            .map(AsRef::as_ref)
            .unwrap_or("")
    }

    /// Local line index of a byte offset in the joined text.
    fn line_of(&self, offset: usize) -> usize {
        self.starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1)
            .min(self.out.len().saturating_sub(1))
    }

    /// Local line range covered by a byte range (inclusive end line).
    fn lines_of(&self, range: &Range<usize>) -> (usize, usize) {
        let first = self.line_of(range.start);
        let last = self.line_of(range.end.saturating_sub(1).max(range.start));
        (first, last)
    }

    fn inline(&self) -> Inline {
        Inline {
            strong: self.strong > 0 || self.cell_is_header(),
            emph: self.emph > 0,
            strike: self.strike > 0,
            link: self.link > 0,
            image: self.image > 0,
        }
    }

    fn cell_is_header(&self) -> bool {
        self.cell.is_some()
            && self
                .table
                .as_ref()
                .and_then(|table| table.rows.last())
                .is_some_and(|row| row.header)
    }

    fn set_leaf(&mut self, first: usize, last: usize, leaf: Leaf) {
        for line in first..=last {
            if let Some(build) = self.out.get_mut(line) {
                if build.leaf.is_none() {
                    build.leaf = Some(leaf.clone());
                }
            }
        }
    }

    fn set_group(&mut self, first: usize, last: usize) {
        for line in first..=last {
            if let Some(build) = self.out.get_mut(line) {
                build.group = Some((first, last + 1));
            }
        }
    }

    fn push_span(&mut self, line: usize, span: Span) {
        if let Some(cell) = self.cell.as_mut() {
            cell.push(span);
        } else if let Some(build) = self.out.get_mut(line) {
            build.content.push(span);
        }
    }

    /// Pushes a verbatim range, split at line ends, as line-relative spans.
    fn push_split(&mut self, range: Range<usize>, kind: SpanKind, inline: Inline) {
        let (first, last) = self.lines_of(&range);
        for line in first..=last {
            let start = self.starts[line];
            let len = self.line_text(line).len();
            let from = range.start.max(start) - start;
            let to = range.end.min(start + len).saturating_sub(start);
            if to > from {
                self.push_span(line, Span::verbatim(from..to, kind, inline));
            }
        }
    }

    fn text(&mut self, text: &str, range: Range<usize>) {
        if self.in_code {
            self.push_split(range, SpanKind::Code, Inline::default());
            return;
        }
        let (first, last) = self.lines_of(&range);
        let line = first;
        let start = self.starts[line];
        let source_text = self.line_text(line);
        let local = (range.start - start)..(range.end.saturating_sub(start)).min(source_text.len());
        let inline = self.inline();
        if first != last || local.start > local.end {
            self.push_split(range, SpanKind::Text, inline);
            return;
        }
        let verbatim = source_text.get(local.clone()) == Some(text);
        if !verbatim {
            self.push_span(
                line,
                Span {
                    source: local,
                    display: Some(text.to_owned()),
                    kind: SpanKind::Text,
                    inline,
                },
            );
            return;
        }
        if inline.link {
            self.push_span(line, Span::verbatim(local, SpanKind::Text, inline));
            return;
        }
        // Bare URLs read as links.
        for (piece, is_url) in split_urls(text) {
            let piece_inline = Inline {
                link: is_url,
                ..inline
            };
            let source = local.start + piece.start..local.start + piece.end;
            self.push_span(line, Span::verbatim(source, SpanKind::Text, piece_inline));
        }
    }

    fn inline_code(&mut self, code: &str, range: Range<usize>) {
        let (first, last) = self.lines_of(&range);
        let start_text = self.line_text(first);
        let start_local = range.start - self.starts[first];
        let ticks = start_text
            .get(start_local..)
            .map_or(0, |rest| rest.bytes().take_while(|&b| b == b'`').count());
        let inner = (range.start + ticks)..range.end.saturating_sub(ticks).max(range.start + ticks);
        let inline = self.inline();
        if first == last {
            let base = self.starts[first];
            let local = inner.start - base..inner.end - base;
            // In a table cell `\|` stands for `|`: show what the parser read.
            let display = (self.cell.is_some() && start_text.get(local.clone()) != Some(code))
                .then(|| code.to_owned());
            self.push_span(
                first,
                Span {
                    source: local,
                    display,
                    kind: SpanKind::Code,
                    inline,
                },
            );
        } else {
            self.push_split(inner, SpanKind::Code, inline);
        }
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        let block = matches!(
            tag,
            Tag::Paragraph
                | Tag::Heading { .. }
                | Tag::BlockQuote(_)
                | Tag::CodeBlock(_)
                | Tag::HtmlBlock
                | Tag::List(_)
                | Tag::Table(_)
                | Tag::FootnoteDefinition(_)
                | Tag::DefinitionList
                | Tag::MetadataBlock(_)
        );
        let (first, last) = self.lines_of(&range);
        if block && self.container_depth == 0 && self.starts.get(first) == Some(&range.start) {
            if let Some(build) = self.out.get_mut(first) {
                build.block_start = true;
            }
        }
        match tag {
            // A list item's own paragraphs are reported only when the list is
            // loose; ignore them so a line's info never depends on whether
            // its list is loose (the leaf falls back to Paragraph anyway).
            Tag::Paragraph if self.parents.last() == Some(&true) => {}
            Tag::Paragraph => self.set_leaf(first, last, Leaf::Paragraph),
            Tag::Heading { level, .. } => {
                let level = heading_level(level);
                if last > first {
                    self.set_leaf(first, last - 1, Leaf::Heading { level });
                    self.set_leaf(last, last, Leaf::SetextUnderline { level });
                } else {
                    self.set_leaf(first, first, Leaf::Heading { level });
                }
            }
            Tag::BlockQuote(_) => {
                self.container_depth += 1;
                self.parents.push(false);
                for line in first..=last {
                    if let Some(build) = self.out.get_mut(line) {
                        build.chain.push(Container::Quote);
                    }
                }
            }
            Tag::List(start) => {
                self.container_depth += 1;
                self.lists.push(start);
            }
            Tag::Item => {
                self.container_depth += 1;
                self.parents.push(true);
                self.item_depth = self.item_depth.saturating_add(1);
                let ordered = self.lists.last().copied().flatten().is_some();
                let line_text = self.line_text(first);
                let col = range.start - self.starts[first];
                let (marker, number) = item_marker(line_text, col, ordered);
                let depth = self.item_depth;
                let marker_text = line_text.get(marker.clone()).unwrap_or("").trim_end();
                let width = if ordered {
                    str_width(marker_text) + 1
                } else {
                    2
                };
                for line in first..=last {
                    if let Some(build) = self.out.get_mut(line) {
                        build.chain.push(Container::Item {
                            marker: (line == first).then(|| marker.clone()),
                            depth,
                            number,
                            width,
                        });
                        build.task = None;
                        build.task_depth = depth;
                    }
                }
                self.items.push((first, last, depth));
                if let Some(build) = self.out.get_mut(first) {
                    build.item = Some(ItemInfo {
                        depth,
                        number,
                        task: None,
                        marker,
                        checkbox: None,
                    });
                }
            }
            Tag::CodeBlock(kind) => {
                self.in_code = true;
                match kind {
                    CodeBlockKind::Fenced(info) => {
                        let lang = info
                            .split_whitespace()
                            .next()
                            .map(str::to_owned)
                            .filter(|lang| !lang.is_empty());
                        self.set_leaf(
                            first,
                            first,
                            Leaf::CodeFence {
                                lang,
                                opening: true,
                            },
                        );
                        let closed = last > first
                            && is_closing_fence(self.line_text(first), self.line_text(last));
                        let inner_last = if closed {
                            self.set_leaf(
                                last,
                                last,
                                Leaf::CodeFence {
                                    lang: None,
                                    opening: false,
                                },
                            );
                            last - 1
                        } else {
                            last
                        };
                        if inner_last > first {
                            self.set_leaf(first + 1, inner_last, Leaf::Code { fenced: true });
                        }
                        self.set_group(first, last);
                    }
                    CodeBlockKind::Indented => {
                        self.set_leaf(first, last, Leaf::Code { fenced: false });
                    }
                }
            }
            Tag::HtmlBlock => self.set_leaf(first, last, Leaf::Html),
            Tag::Table(aligns) => {
                self.set_group(first, last);
                self.table = Some(TableBuild {
                    aligns,
                    first,
                    last,
                    rows: Vec::new(),
                });
            }
            Tag::TableHead | Tag::TableRow => {
                let header = matches!(tag, Tag::TableHead);
                let end = range.end.saturating_sub(self.starts[first]);
                if let Some(table) = self.table.as_mut() {
                    table.rows.push(RowBuild {
                        line: first,
                        header,
                        cells: Vec::new(),
                        end,
                    });
                }
                self.set_leaf(first, first, Leaf::TableRow { header });
            }
            Tag::TableCell => self.cell = Some(Vec::new()),
            Tag::Emphasis => self.emph += 1,
            Tag::Strong => self.strong += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { .. } => self.link += 1,
            Tag::Image { .. } => {
                self.image += 1;
                let local = range.start - self.starts[first];
                self.push_span(
                    first,
                    Span::shown(local..local, "▣ ", SpanKind::ImageMarker),
                );
            }
            Tag::FootnoteDefinition(_) | Tag::DefinitionList => {
                self.container_depth += 1;
                self.parents.push(false);
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::BlockQuote(_) | TagEnd::FootnoteDefinition | TagEnd::DefinitionList => {
                self.container_depth = self.container_depth.saturating_sub(1);
                self.parents.pop();
            }
            TagEnd::List(_) => {
                self.container_depth = self.container_depth.saturating_sub(1);
                self.lists.pop();
            }
            TagEnd::Item => {
                self.container_depth = self.container_depth.saturating_sub(1);
                self.item_depth = self.item_depth.saturating_sub(1);
                self.items.pop();
                self.parents.pop();
            }
            TagEnd::CodeBlock => self.in_code = false,
            TagEnd::TableCell => {
                if let Some(cell) = self.cell.take() {
                    if let Some(row) = self.table.as_mut().and_then(|table| table.rows.last_mut()) {
                        row.cells.push(cell);
                    }
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.finish_table(table);
                }
            }
            TagEnd::Emphasis => self.emph = self.emph.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => self.link = self.link.saturating_sub(1),
            TagEnd::Image => self.image = self.image.saturating_sub(1),
            _ => {}
        }
    }

    fn finish_table(&mut self, table: TableBuild) {
        let columns = table
            .rows
            .iter()
            .map(|row| row.cells.len())
            .max()
            .unwrap_or(0)
            .max(table.aligns.len());
        let mut widths = vec![1usize; columns];
        for row in &table.rows {
            let text = self.line_text(row.line);
            for (column, cell) in row.cells.iter().enumerate() {
                let width: usize = cell.iter().map(|span| str_width(span.text(text))).sum();
                widths[column] = widths[column].max(width);
            }
        }
        let mut row_lines = Vec::new();
        for row in &table.rows {
            row_lines.push(row.line);
            let text = self.line_text(row.line);
            let mut spans = Vec::new();
            for (column, &width) in widths.iter().enumerate() {
                let cell = row.cells.get(column).map(Vec::as_slice).unwrap_or(&[]);
                let at = cell.first().map_or(row.end, |span| span.source.start);
                let end = cell.last().map_or(row.end, |span| span.source.end);
                let sep = if column == 0 { "│ " } else { " │ " };
                spans.push(Span::shown(at..at, sep, SpanKind::TableSep));
                let used: usize = cell.iter().map(|span| str_width(span.text(text))).sum();
                let pad = width.saturating_sub(used);
                let (left, right) = match table.aligns.get(column) {
                    Some(Alignment::Right) => (pad, 0),
                    Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                if left > 0 {
                    spans.push(Span::shown(at..at, " ".repeat(left), SpanKind::TablePad));
                }
                spans.extend(cell.iter().cloned());
                if right > 0 {
                    spans.push(Span::shown(end..end, " ".repeat(right), SpanKind::TablePad));
                }
            }
            spans.push(Span::shown(row.end..row.end, " │", SpanKind::TableSep));
            if let Some(build) = self.out.get_mut(row.line) {
                build.composed = Some(spans);
            }
        }
        // Lines without a row are the delimiter row.
        let border: String = {
            let parts: Vec<String> = widths.iter().map(|width| "─".repeat(width + 2)).collect();
            format!("├{}┤", parts.join("┼"))
        };
        for line in table.first..=table.last {
            if row_lines.contains(&line) {
                continue;
            }
            let len = self.line_text(line).len();
            if let Some(build) = self.out.get_mut(line) {
                build.leaf = Some(Leaf::TableDelimiter);
                build.composed = Some(vec![Span::shown(
                    0..len,
                    border.clone(),
                    SpanKind::TableSep,
                )]);
            }
        }
    }

    /// Marks every line of the innermost open item (not its sub-items) with
    /// the item's task state.
    fn mark_task(&mut self, checked: bool) {
        let Some(&(first, last, depth)) = self.items.last() else {
            return;
        };
        for line in first..=last {
            if let Some(build) = self.out.get_mut(line) {
                if build.task_depth == depth {
                    build.task = Some(checked);
                }
            }
        }
    }

    fn finish(self) -> Vec<LineInfo> {
        let mut infos = Vec::with_capacity(self.out.len());
        for (local, build) in self.out.into_iter().enumerate() {
            let text = self
                .lines
                .get(self.first + local)
                .map(AsRef::as_ref)
                .unwrap_or("");
            infos.push(finish_line(text, local, build));
        }
        infos
    }
}

fn finish_line(text: &str, local: usize, build: LineBuild) -> LineInfo {
    let mut prefix = Vec::new();
    let mut cont = Vec::new();
    let mut pos = 0;
    let mut quote_depth = 0u8;
    let mut item_depth = 0u8;
    for container in &build.chain {
        match container {
            Container::Quote => {
                quote_depth = quote_depth.saturating_add(1);
                let rest = &text[pos.min(text.len())..];
                let skipped = rest.len() - rest.trim_start_matches([' ', '\t', '\u{feff}']).len();
                let source = if rest[skipped..].starts_with('>') {
                    let start = pos + skipped;
                    let mut end = start + 1;
                    if text[end..].starts_with(' ') {
                        end += 1;
                    }
                    pos = end;
                    start..end
                } else {
                    pos..pos
                };
                prefix.push(Span::shown(source, "▎ ", SpanKind::QuoteBar));
                cont.push(Span::shown(0..0, "▎ ", SpanKind::QuoteBar));
            }
            Container::Item {
                marker,
                depth,
                number,
                width,
            } => {
                item_depth = item_depth.saturating_add(1);
                match marker {
                    Some(marker) => {
                        let span = match number {
                            Some(_) => {
                                let shown = text.get(marker.clone()).unwrap_or("").trim_end();
                                Span::shown(marker.clone(), format!("{shown} "), SpanKind::Number)
                            }
                            None => {
                                let glyph = BULLETS[(usize::from(*depth).saturating_sub(1)) % 3];
                                Span::shown(marker.clone(), glyph, SpanKind::Bullet)
                            }
                        };
                        pos = pos.max(marker.end);
                        prefix.push(span);
                    }
                    None => {
                        prefix.push(Span::shown(pos..pos, " ".repeat(*width), SpanKind::Indent))
                    }
                }
                cont.push(Span::shown(0..0, " ".repeat(*width), SpanKind::Indent));
            }
        }
    }
    let mut item = build.item;
    let mut content = build.content;
    for span in &mut content {
        if let SpanKind::Checkbox { .. } = span.kind {
            if let Some(item) = item.as_mut() {
                item.checkbox = Some(span.source.clone());
            }
        }
    }
    if item.as_ref().is_some_and(|item| item.checkbox.is_some()) {
        if prefix
            .last()
            .is_some_and(|span| span.kind == SpanKind::Bullet)
        {
            // A bullet task shows only its box (`☐ todo`, not `• ☐ todo`),
            // which takes the bullet's place, so wrapped rows already line
            // up. An ordered task keeps its number.
            prefix.pop();
        } else {
            cont.push(Span::shown(0..0, "  ", SpanKind::Indent));
        }
    }
    let leaf = match build.leaf {
        Some(leaf) => leaf,
        None => {
            let rest = text.get(pos..).unwrap_or("");
            if rest.trim().is_empty() && content.is_empty() {
                Leaf::Blank
            } else if !content.is_empty() {
                // Text of a tight list item (no paragraph event); the same
                // leaf a loose list's paragraph gets, so a list's looseness
                // never changes a line's info.
                Leaf::Paragraph
            } else {
                if content.is_empty() {
                    let start = pos + (rest.len() - rest.trim_start().len());
                    content.push(Span::verbatim(
                        start..text.len(),
                        SpanKind::Marker,
                        Inline::default(),
                    ));
                }
                Leaf::Other
            }
        }
    };
    let spans = match build.composed {
        Some(composed) => {
            prefix.extend(composed);
            prefix
        }
        None => {
            content.sort_by_key(|span| span.source.start);
            prefix.extend(content);
            prefix
        }
    };
    let (group_back, group_len) = match build.group {
        Some((start, end)) => ((local - start) as u32, (end - start) as u32),
        None => (0, 0),
    };
    let ref_def = text
        .get(pos..)
        .filter(|_| leaf == Leaf::Other)
        .map(|rest| pos + (rest.len() - rest.trim_start().len()));
    LineInfo {
        leaf,
        quote_depth,
        item_depth,
        item,
        task: build.task,
        spans,
        cont,
        hidden: false,
        group_back,
        group_len,
        block_start: build.block_start,
        ref_def,
    }
}

/// The marker range (with the spaces after it) of a list item starting at
/// `col`, and its number when ordered.
fn item_marker(line: &str, col: usize, ordered: bool) -> (Range<usize>, Option<u64>) {
    let rest = line.get(col..).unwrap_or("");
    let (len, number) = if ordered {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let delimiter = rest[digits..].starts_with(['.', ')']);
        if digits == 0 || !delimiter {
            return (col..col, None);
        }
        (digits + 1, rest[..digits].parse().ok())
    } else if rest.starts_with(['-', '*', '+']) {
        (1, None)
    } else {
        return (col..col, None);
    };
    let after = &rest[len..];
    let spaces = after
        .bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count();
    let spaces = if spaces == after.len() {
        spaces
    } else if spaces > 4 {
        1
    } else {
        spaces
    };
    (col..col + len + spaces, number.or(ordered.then_some(0)))
}

/// Whether `line` closes the fence opened by `opening`: the same fence
/// character, at least as many of them, nothing else on the line.
fn is_closing_fence(opening: &str, line: &str) -> bool {
    let strip = |text: &str| text.trim_start_matches([' ', '\t', '>']).to_owned();
    let opening = strip(opening);
    let Some(fence) = opening.chars().next().filter(|ch| matches!(ch, '`' | '~')) else {
        return false;
    };
    let open_len = opening.chars().take_while(|&ch| ch == fence).count();
    let closing = strip(line);
    let closing = closing.trim_end();
    closing.chars().count() >= open_len.max(3) && closing.chars().all(|ch| ch == fence)
}

/// Splits text into pieces, marking bare `http(s)://` URLs.
fn split_urls(text: &str) -> Vec<(Range<usize>, bool)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(found) = find_url_start(&text[pos..]) {
        let start = pos + found;
        let tail = &text[start..];
        let mut len = tail
            .find(|ch: char| ch.is_whitespace() || ch == '<' || ch == '>')
            .unwrap_or(tail.len());
        while len > 0 && tail[..len].ends_with(['.', ',', ';', ':', '!', '?', ')', '\'', '"']) {
            len -= 1;
        }
        let scheme = if tail.starts_with("https://") { 8 } else { 7 };
        if len <= scheme {
            out.push((pos..start + scheme, false));
            pos = start + scheme;
            continue;
        }
        if start > pos {
            out.push((pos..start, false));
        }
        out.push((start..start + len, true));
        pos = start + len;
    }
    if pos < text.len() || out.is_empty() {
        out.push((pos..text.len(), false));
    }
    out
}

fn find_url_start(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(found) = text[from..].find("http") {
        let at = from + found;
        let rest = &text[at..];
        let boundary = at == 0 || !text[..at].ends_with(|ch: char| ch.is_alphanumeric());
        if boundary && (rest.starts_with("https://") || rest.starts_with("http://")) {
            return Some(at);
        }
        from = at + 4;
    }
    None
}

/// Parses `lines[first..last]` as a standalone Markdown text. `prelude`
/// (link reference definitions from elsewhere in the note, or empty) is
/// parsed first so links resolve, but produces no lines.
fn parse_lines<S: AsRef<str>>(
    lines: &[S],
    first: usize,
    last: usize,
    prelude: &str,
) -> Vec<LineInfo> {
    if first >= last {
        return Vec::new();
    }
    let mut text = String::with_capacity(prelude.len());
    text.push_str(prelude);
    let base = text.len();
    let mut starts = Vec::with_capacity(last - first);
    for (index, line) in lines[first..last].iter().enumerate() {
        starts.push(text.len());
        let line = line.as_ref();
        match line.strip_prefix('\u{feff}') {
            // A byte-order mark on the note's first line: three spaces of the
            // same length keep every offset and still allow a heading, list
            // or fence to start the line.
            Some(rest) if first + index == 0 => {
                text.push_str("   ");
                text.push_str(rest);
            }
            _ => text.push_str(line),
        }
        text.push('\n');
    }
    let mut builder = Builder {
        lines,
        first,
        starts,
        text_len: text.len(),
        out: vec![LineBuild::default(); last - first],
        container_depth: 0,
        item_depth: 0,
        lists: Vec::new(),
        strong: 0,
        emph: 0,
        strike: 0,
        link: 0,
        image: 0,
        in_code: false,
        table: None,
        cell: None,
        items: Vec::new(),
        parents: Vec::new(),
    };
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    for (event, range) in Parser::new_ext(&text, options).into_offset_iter() {
        if range.start < base {
            // The prelude (definitions only) emits nothing that is drawn.
            continue;
        }
        let range = range.start.min(builder.text_len)..range.end.min(builder.text_len);
        match event {
            Event::Start(tag) => builder.start(tag, range),
            Event::End(tag) => builder.end(tag),
            Event::Text(content) => builder.text(&content, range),
            Event::Code(content) => builder.inline_code(&content, range),
            Event::InlineHtml(_) | Event::Html(_) => {
                builder.push_split(range, SpanKind::Html, Inline::default())
            }
            Event::InlineMath(content) | Event::DisplayMath(content) => {
                builder.text(&content, range)
            }
            Event::FootnoteReference(_) => {
                builder.push_split(range, SpanKind::Text, Inline::default())
            }
            Event::TaskListMarker(checked) => {
                let line = builder.line_of(range.start);
                let base = builder.starts[line];
                let local = range.start - base..range.end - base;
                let glyph = if checked { "☑ " } else { "☐ " };
                builder.push_span(
                    line,
                    Span::shown(local, glyph, SpanKind::Checkbox { checked }),
                );
                if let Some(item) = builder
                    .out
                    .get_mut(line)
                    .and_then(|build| build.item.as_mut())
                {
                    item.task = Some(checked);
                }
                builder.mark_task(checked);
            }
            Event::Rule => {
                let (first, last) = builder.lines_of(&range);
                builder.set_leaf(first, last, Leaf::Rule);
                if builder.container_depth == 0 && builder.starts.get(first) == Some(&range.start) {
                    builder.out[first].block_start = true;
                }
            }
            Event::SoftBreak | Event::HardBreak => {}
        }
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> (Vec<String>, Document) {
        let lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let document = Document::parse(&lines);
        (lines, document)
    }

    /// The rendered line as plain text.
    fn rendered(lines: &[String], document: &Document, index: usize) -> String {
        let line = &lines[index];
        document.lines[index]
            .spans
            .iter()
            .map(|span| span.text(line))
            .collect()
    }

    fn texts(lines: &[String], document: &Document) -> Vec<String> {
        (0..lines.len())
            .map(|index| rendered(lines, document, index))
            .collect()
    }

    #[test]
    fn headings_hide_markers_and_keep_source_ranges() {
        let (lines, document) = doc("# Title\n\nSub\n===\n### Three ###");
        assert_eq!(document.lines[0].leaf, Leaf::Heading { level: 1 });
        assert_eq!(rendered(&lines, &document, 0), "Title");
        assert_eq!(document.lines[0].spans[0].source, 2..7);
        assert_eq!(document.lines[1].leaf, Leaf::Blank);
        assert_eq!(document.lines[2].leaf, Leaf::Heading { level: 1 });
        assert_eq!(document.lines[3].leaf, Leaf::SetextUnderline { level: 1 });
        assert_eq!(rendered(&lines, &document, 3), "");
        assert_eq!(document.lines[4].leaf, Leaf::Heading { level: 3 });
        assert_eq!(rendered(&lines, &document, 4), "Three");
    }

    #[test]
    fn inline_styles_and_concealed_markers() {
        let (lines, document) =
            doc("a **b** *c* ~~d~~ `e` [f](http://x) <http://y> https://z.io/p. &amp; \\*");
        let line = &lines[0];
        let spans = &document.lines[0].spans;
        let find = |text: &str| {
            spans
                .iter()
                .find(|span| span.text(line) == text)
                .unwrap_or_else(|| panic!("no span {text:?} in {spans:?}"))
        };
        assert!(find("b").inline.strong);
        assert!(find("c").inline.emph);
        assert!(find("d").inline.strike);
        assert_eq!(find("e").kind, SpanKind::Code);
        assert_eq!(find("e").source, 19..20);
        assert!(find("f").inline.link);
        assert!(find("http://y").inline.link);
        assert!(find("https://z.io/p").inline.link);
        assert!(!find(". ").inline.link);
        let amp = find("&");
        assert!(!amp.is_verbatim());
        assert_eq!(&line[amp.source.clone()], "&amp;");
        assert_eq!(
            rendered(&lines, &document, 0),
            "a b c d e f http://y https://z.io/p. & *"
        );
        // Every verbatim span's text is its source.
        for span in spans.iter().filter(|span| span.is_verbatim()) {
            assert_eq!(span.text(line), &line[span.source.clone()]);
        }
    }

    #[test]
    fn lists_bullets_numbers_tasks_and_nesting() {
        let (lines, document) =
            doc("- one\n  - two\n    - three\n      - four\n1. first\n2) second\n- [ ] todo\n- [x] done\n  more");
        let texts = texts(&lines, &document);
        assert_eq!(texts[0], "• one");
        assert_eq!(texts[1], "  ◦ two");
        assert_eq!(texts[2], "    ▪ three");
        assert_eq!(texts[3], "      • four");
        assert_eq!(texts[4], "1. first");
        assert_eq!(texts[5], "2) second");
        assert_eq!(texts[6], "☐ todo");
        assert_eq!(texts[7], "☑ done");
        assert_eq!(texts[8], "  more");
        let item = document.lines[1].item.as_ref().unwrap();
        assert_eq!(item.depth, 2);
        assert_eq!(item.marker, 2..4);
        assert_eq!(document.lines[4].item.as_ref().unwrap().number, Some(1));
        assert_eq!(document.lines[5].item.as_ref().unwrap().number, Some(2));
        let item = document.lines[6].item.as_ref().unwrap();
        assert_eq!(
            (item.checkbox.clone(), item.task),
            (Some(2..5), Some(false))
        );
        let item = document.lines[7].item.as_ref().unwrap();
        assert_eq!((item.checkbox.clone(), item.task), (Some(2..5), Some(true)));
        // Every line of a checked task knows it, not just the first.
        assert_eq!(document.lines[8].task, Some(true));
        assert_eq!(document.lines[6].task, Some(false));
        assert_eq!(document.lines[0].task, None);
        assert_eq!(document.lines[8].item_depth, 1);
        // Wrapped rows of a task item indent past the checkbox, which takes
        // the bullet's place; an ordered task keeps its number.
        let cont: String = document.lines[7].cont.iter().map(|s| s.text("")).collect();
        assert_eq!(cont, "  ");
        let (lines, document) = doc("1. [ ] first\n   - [x] nested");
        let shown = self::texts(&lines, &document);
        assert_eq!(shown[0], "1. ☐ first");
        assert_eq!(shown[1], "   ☑ nested");
        let cont: String = document.lines[0].cont.iter().map(|s| s.text("")).collect();
        assert_eq!(cont, "     ");
    }

    #[test]
    fn quotes_rules_and_html() {
        let (lines, document) = doc("> quoted\n> > nested\nlazy\n\n---\n\n<div>\nhi\n</div>");
        let texts = texts(&lines, &document);
        assert_eq!(texts[0], "▎ quoted");
        assert_eq!(texts[1], "▎ ▎ nested");
        assert_eq!(document.lines[1].quote_depth, 2);
        assert_eq!(document.lines[0].spans[0].source, 0..2);
        assert_eq!(document.lines[4].leaf, Leaf::Rule);
        assert_eq!(document.lines[6].leaf, Leaf::Html);
        assert_eq!(texts[7], "hi");
    }

    #[test]
    fn fenced_code_blocks_group_and_label() {
        let (lines, document) = doc("text\n\n```rust\nfn main() {}\n\n```\nafter");
        assert_eq!(
            document.lines[2].leaf,
            Leaf::CodeFence {
                lang: Some("rust".into()),
                opening: true
            }
        );
        assert_eq!(document.lines[3].leaf, Leaf::Code { fenced: true });
        assert_eq!(document.lines[4].leaf, Leaf::Code { fenced: true });
        assert_eq!(
            document.lines[5].leaf,
            Leaf::CodeFence {
                lang: None,
                opening: false
            }
        );
        assert_eq!(rendered(&lines, &document, 3), "fn main() {}");
        assert_eq!(rendered(&lines, &document, 2), "");
        for index in 2..=5 {
            assert_eq!(document.raw_range(index), 2..6);
        }
        assert_eq!(document.raw_range(0), 0..1);
        assert_eq!(document.lines[6].leaf, Leaf::Paragraph);

        // Unclosed fence runs to the end.
        let (_, document) = doc("```\na\nb");
        assert_eq!(document.raw_range(2), 0..3);
        assert_eq!(document.lines[2].leaf, Leaf::Code { fenced: true });
    }

    #[test]
    fn tables_align_with_box_drawing() {
        let (lines, document) = doc("| a | long |\n|---|:---:|\n| xx | y |\n\nafter");
        let texts = texts(&lines, &document);
        assert_eq!(texts[0], "│ a  │ long │");
        assert_eq!(texts[1], "├────┼──────┤");
        assert_eq!(texts[2], "│ xx │  y   │");
        assert_eq!(document.lines[0].leaf, Leaf::TableRow { header: true });
        assert_eq!(document.lines[1].leaf, Leaf::TableDelimiter);
        assert_eq!(document.lines[2].leaf, Leaf::TableRow { header: false });
        assert!(document.lines[0]
            .spans
            .iter()
            .any(|span| span.text(&lines[0]) == "a" && span.inline.strong));
        assert_eq!(document.raw_range(1), 0..3);
        assert_eq!(document.raw_range(4), 4..5);
    }

    #[test]
    fn front_matter_collapses_to_one_line() {
        let (lines, document) =
            doc("+++\ntitle = \"T\"\ntags = [\"a\", \"b\"]\n+++\n# Heading\nbody");
        assert_eq!(rendered(&lines, &document, 0), "T · a, b");
        assert!(!document.lines[0].hidden);
        assert!(document.lines[1].hidden && document.lines[3].hidden);
        assert_eq!(document.raw_range(2), 0..4);
        assert_eq!(document.lines[4].leaf, Leaf::Heading { level: 1 });
        assert_eq!(document.title(&lines).as_deref(), Some("T"));
        let (lines, document) = doc("\n# Only heading\n");
        assert_eq!(document.title(&lines).as_deref(), Some("Only heading"));
        // YAML front matter is not a setext heading or a rule.
        let (_, document) = doc("---\ntitle: x\n---\ntext");
        assert_eq!(
            document.front_matter().unwrap().matter.title.as_deref(),
            Some("x")
        );
        assert_eq!(document.lines[3].leaf, Leaf::Paragraph);
    }

    #[test]
    fn raw_spans_cover_the_source_with_markers_tinted() {
        let (lines, document) = doc("## **Bold** and `code`");
        let raw = raw_spans(&lines[0], &document.lines[0]);
        let text: String = raw.iter().map(|span| span.text(&lines[0])).collect();
        assert_eq!(text, lines[0]);
        assert!(raw.iter().all(Span::is_verbatim));
        assert_eq!(raw[0].kind, SpanKind::Marker);
        assert_eq!(raw[0].text(&lines[0]), "## **");
        let bold = raw
            .iter()
            .find(|span| span.text(&lines[0]) == "Bold")
            .unwrap();
        assert!(bold.inline.strong);
        // Raw of the table delimiter row and the collapsed front matter.
        let (lines, document) = doc("+++\nx = 1\n+++\n| a |\n|---|\n| b |");
        for (line, info) in lines.iter().zip(document.lines()) {
            let raw = raw_spans(line, info);
            let text: String = raw.iter().map(|span| span.text(line)).collect();
            assert_eq!(&text, line);
        }
    }

    #[test]
    fn unicode_and_tabs_survive() {
        let (lines, document) = doc("- 中文 **😀**\n\tindented code");
        assert_eq!(rendered(&lines, &document, 0), "• 中文 😀");
        let (lines, document) = doc("para\n\n\tcode\there");
        assert_eq!(document.lines[2].leaf, Leaf::Code { fenced: false });
        assert!(rendered(&lines, &document, 2).contains("code\there"));
    }

    #[test]
    fn reference_definitions_are_shown_dimmed() {
        let (lines, document) = doc("[a][r]\n\n[r]: http://x");
        assert_eq!(document.lines[2].leaf, Leaf::Other);
        assert_eq!(rendered(&lines, &document, 2), "[r]: http://x");
        assert!(document.lines[0].spans.iter().any(|span| span.inline.link));
        assert!(document.has_refs);
    }

    /// Rounds for the randomised tests (`LUMBRIDGE_NOTES_FUZZ_ROUNDS` runs more).
    fn fuzz_rounds(default: usize) -> usize {
        std::env::var("LUMBRIDGE_NOTES_FUZZ_ROUNDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    }

    /// Mixed into the randomised tests' seeds (`LUMBRIDGE_NOTES_FUZZ_SEED`).
    fn fuzz_seed() -> u64 {
        std::env::var("LUMBRIDGE_NOTES_FUZZ_SEED")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    }

    /// Applies `edit` to `lines`, returning the damage like the buffer would.
    fn splice(lines: &mut Vec<String>, start: usize, remove: usize, insert: &[&str]) -> Damage {
        lines.splice(start..start + remove, insert.iter().map(|s| s.to_string()));
        Damage {
            start,
            old_end: start + remove,
            new_end: start + insert.len(),
        }
    }

    fn check_incremental(text: &str, edits: &[(usize, usize, &[&str])]) {
        let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let mut document = Document::parse(&lines);
        for &(start, remove, insert) in edits {
            let damage = splice(&mut lines, start, remove, insert);
            let changed = document.update(&lines, damage);
            assert!(changed.start <= damage.start, "{changed:?} vs {damage:?}");
            let full = Document::parse(&lines);
            assert_eq!(
                document, full,
                "incremental != full after {start},{remove},{insert:?}\n{lines:#?}"
            );
        }
    }

    #[test]
    fn incremental_matches_full_parse() {
        let base = "# T\n\npara one\nstill para\n\n- a\n- b\n\n  cont\n\n> q\n\n| x | y |\n|---|---|\n| 1 | 2 |\n\n```\ncode\n```\n\nlast";
        check_incremental(base, &[(2, 1, &["changed **para**"])]);
        // Opening a fence swallows the rest; closing it restores.
        check_incremental(base, &[(4, 1, &["```"]), (4, 1, &[""])]);
        // Joining blocks by removing a blank line.
        check_incremental(base, &[(4, 1, &[]), (3, 0, &[""])]);
        // Turning a paragraph into a list item adjacent to a list.
        check_incremental(base, &[(9, 1, &["- c"])]);
        // Deleting a table delimiter.
        check_incremental(base, &[(13, 1, &[])]);
        // Editing inside a code block.
        check_incremental(base, &[(17, 1, &["code", "more", "```"])]);
        // Html comment that spans blank lines.
        check_incremental(base, &[(10, 1, &["<!--"]), (19, 1, &["-->"])]);
        // Front matter appears and changes.
        check_incremental(
            base,
            &[
                (0, 0, &["+++", "title = \"x\"", "+++"]),
                (1, 1, &["title = \"y\""]),
            ],
        );
        // Unclosed front matter fence getting closed later in the file.
        check_incremental("+++\na\n\nb\n\nc", &[(4, 1, &["+++"])]);
        // Indented code right after a blank.
        check_incremental(base, &[(9, 1, &["    code"])]);
        // Setext heading created by an underline.
        check_incremental(base, &[(3, 1, &["==="])]);
    }

    #[test]
    fn incremental_randomised_against_full_parse() {
        let rounds = fuzz_rounds(400);
        let pieces: [&str; 34] = [
            "",
            "",
            "text",
            "# h",
            "- item",
            "  - sub",
            "1. num",
            "2. two",
            "> quote",
            "```",
            "| a | b |",
            "|---|---|",
            "---",
            "===",
            "    code",
            "<!--",
            "-->",
            "- [ ] task",
            "> - quoted item",
            "  lazy",
            "~~~",
            "<div>",
            "- ![](img)",
            "- a",
            "- b",
            "* other",
            "1. one",
            "[r]: http://x",
            "see [a][r]",
            "+++",
            "title: t",
            "```rust",
            "\u{feff}# bom",
            "  - [x] done",
        ];
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15 ^ fuzz_seed();
        let mut next = move |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        for _round in 0..rounds {
            let mut lines: Vec<String> = (0..30)
                .map(|_| pieces[next(pieces.len())].to_owned())
                .collect();
            let mut document = Document::parse(&lines);
            for _ in 0..25 {
                let start = next(lines.len() + 1).min(lines.len());
                let remove = next(3).min(lines.len() - start);
                let count = next(3);
                let insert: Vec<&str> = (0..count).map(|_| pieces[next(pieces.len())]).collect();
                if lines.len() - remove + insert.len() == 0 {
                    continue;
                }
                let before = lines.clone();
                let damage = splice(&mut lines, start, remove, &insert);
                document.update(&lines, damage);
                let full = Document::parse(&lines);
                if document != full {
                    let first =
                        (0..lines.len()).find(|&i| document.lines.get(i) != full.lines.get(i));
                    panic!(
                        "before {before:#?}\ndamage {damage:?}\nafter {lines:#?}\nfirst diff {first:?}\ninc {:#?}\nfull {:#?}",
                        first.and_then(|i| document.lines.get(i)),
                        first.and_then(|i| full.lines.get(i))
                    );
                }
            }
        }
    }

    /// Typing-like edits (one line changed, split or joined) in documents
    /// full of long lists, code blocks and front matter, the shapes the
    /// fast paths are for.
    #[test]
    fn incremental_typing_matches_full_parse() {
        let pieces: [&str; 16] = [
            "- item",
            "- [ ] task",
            "  - sub",
            "1. one",
            "",
            "```",
            "code",
            "text",
            "> q",
            "---",
            "title: x",
            "+++",
            "|a|b|",
            "|-|-|",
            "* star",
            "[r]: u",
        ];
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d ^ fuzz_seed();
        let mut next = move |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        let inserts = ["x", "`", "*", "[ ]", "|", "```", " ", "-", "# ", "[r]"];
        for _round in 0..fuzz_rounds(300) {
            let mut lines: Vec<String> = (0..40)
                .map(|_| pieces[next(pieces.len())].to_owned())
                .collect();
            let mut document = Document::parse(&lines);
            for _ in 0..30 {
                let line = next(lines.len());
                let before = lines.clone();
                let damage = match next(4) {
                    0 | 1 => {
                        // Type at a char boundary of the line.
                        let text = &mut lines[line];
                        let mut at = next(text.len() + 1);
                        while !text.is_char_boundary(at) {
                            at -= 1;
                        }
                        text.insert_str(at, inserts[next(inserts.len())]);
                        Damage {
                            start: line,
                            old_end: line + 1,
                            new_end: line + 1,
                        }
                    }
                    2 => {
                        // Split the line (Enter).
                        let text = lines[line].clone();
                        let mut at = next(text.len() + 1);
                        while !text.is_char_boundary(at) {
                            at -= 1;
                        }
                        lines[line] = text[..at].to_owned();
                        lines.insert(line + 1, text[at..].to_owned());
                        Damage {
                            start: line,
                            old_end: line + 1,
                            new_end: line + 2,
                        }
                    }
                    _ => {
                        if line + 1 >= lines.len() {
                            continue;
                        }
                        // Join with the next line (Backspace at its start).
                        let tail = lines.remove(line + 1);
                        lines[line].push_str(&tail);
                        Damage {
                            start: line,
                            old_end: line + 2,
                            new_end: line + 1,
                        }
                    }
                };
                document.update(&lines, damage);
                let full = Document::parse(&lines);
                if document != full {
                    let first =
                        (0..lines.len()).find(|&i| document.lines.get(i) != full.lines.get(i));
                    panic!(
                        "before {before:#?}\ndamage {damage:?}\nafter {lines:#?}\nfirst diff {first:?}\ninc {:#?}\nfull {:#?}",
                        first.and_then(|i| document.lines.get(i)),
                        first.and_then(|i| full.lines.get(i))
                    );
                }
            }
        }
    }

    /// Applies one edit and returns what the update re-parsed.
    fn edit_once(lines: &mut [String], document: &mut Document, damage: Damage) -> Damage {
        let changed = document.update(lines, damage);
        assert_eq!(*document, Document::parse(lines));
        changed
    }

    #[test]
    fn common_edits_in_big_notes_stay_local() {
        let local = |changed: Damage, around: usize| {
            changed.start + 50 >= around && changed.old_end <= around + 50
        };
        // Typing in the `# Title` on line 0.
        let mut lines = vec!["# Title".to_owned(), String::new()];
        for index in 0..20_000 {
            lines.push(format!("Paragraph {index}."));
            lines.push(String::new());
        }
        let mut document = Document::parse(&lines);
        lines[0] = "# Title!".to_owned();
        let one = Damage {
            start: 0,
            old_end: 1,
            new_end: 1,
        };
        let changed = edit_once(&mut lines, &mut document, one);
        assert!(local(changed, 0), "line 0: {changed:?}");

        // A link reference definition anywhere no longer forces a full parse,
        // and links still resolve to it.
        lines.push("[r]: http://x".to_owned());
        document = Document::parse(&lines);
        lines[20_001] = "see [it][r] now".to_owned();
        let mid = Damage {
            start: 20_001,
            old_end: 20_002,
            new_end: 20_002,
        };
        let changed = edit_once(&mut lines, &mut document, mid);
        assert!(local(changed, 20_001), "refs: {changed:?}");
        assert!(document.lines[20_001]
            .spans
            .iter()
            .any(|span| span.inline.link));

        // The middle of a long tight list.
        let mut lines: Vec<String> = (0..40_000).map(|i| format!("- [ ] item {i}")).collect();
        let mut document = Document::parse(&lines);
        lines[20_000].push('!');
        let changed = edit_once(
            &mut lines,
            &mut document,
            Damage {
                start: 20_000,
                old_end: 20_001,
                new_end: 20_001,
            },
        );
        assert!(local(changed, 20_000), "list: {changed:?}");

        // Inside front matter, and inside a long fenced code block.
        let mut lines = vec![
            "+++".to_owned(),
            "title = \"T\"".to_owned(),
            "+++".to_owned(),
            "```".to_owned(),
        ];
        lines.extend((0..40_000).map(|i| format!("code {i}")));
        lines.push("```".to_owned());
        let mut document = Document::parse(&lines);
        lines[1] = "title = \"Tx\"".to_owned();
        let changed = edit_once(
            &mut lines,
            &mut document,
            Damage {
                start: 1,
                old_end: 2,
                new_end: 2,
            },
        );
        assert_eq!(
            changed,
            Damage {
                start: 0,
                old_end: 3,
                new_end: 3
            }
        );
        assert_eq!(
            document.front_matter().unwrap().matter.title.as_deref(),
            Some("Tx")
        );
        lines.insert(20_000, "new code".to_owned());
        let changed = edit_once(
            &mut lines,
            &mut document,
            Damage {
                start: 20_000,
                old_end: 20_000,
                new_end: 20_001,
            },
        );
        assert_eq!(
            changed,
            Damage {
                start: 20_000,
                old_end: 20_000,
                new_end: 20_001
            }
        );
        assert_eq!(document.raw_range(20_000), 3..40_006);
    }

    #[test]
    fn a_leading_thematic_break_is_not_front_matter() {
        let (lines, document) = doc("---\n\n# Real heading\n\nbody\n\n---\n\nend");
        assert!(document.front_matter().is_none());
        assert_eq!(document.lines[0].leaf, Leaf::Rule);
        assert!(!document.lines[2].hidden);
        assert_eq!(rendered(&lines, &document, 2), "Real heading");
        assert_eq!(rendered(&lines, &document, 4), "body");
    }

    #[test]
    fn titles_come_from_the_rendered_heading() {
        let (lines, document) = doc("# **Bold** `code` &amp; title\n\ntext");
        assert_eq!(document.title(&lines).as_deref(), Some("Bold code & title"));
        let (lines, document) = doc("Setext title\n===\n\ntext");
        assert_eq!(document.title(&lines).as_deref(), Some("Setext title"));
        let (lines, document) = doc("## Not level one\n");
        assert_eq!(document.title(&lines), None);
        let (lines, document) = doc("> # quoted\n");
        assert_eq!(document.title(&lines), None);
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_a_heading() {
        let (lines, document) = doc("\u{feff}# BOM heading\n\nbody");
        assert_eq!(document.lines[0].leaf, Leaf::Heading { level: 1 });
        assert_eq!(rendered(&lines, &document, 0), "BOM heading");
        assert_eq!(document.title(&lines).as_deref(), Some("BOM heading"));
        let (lines, document) = doc("\u{feff}- item\n> quote");
        assert_eq!(rendered(&lines, &document, 0), "• item");
        // Raw view still covers every byte.
        let raw = raw_spans(&lines[0], &document.lines[0]);
        let text: String = raw.iter().map(|span| span.text(&lines[0])).collect();
        assert_eq!(text, lines[0]);
    }

    #[test]
    fn escaped_pipes_in_table_code_show_as_pipes() {
        let (lines, document) = doc("| a |\n|---|\n| `x\\|y` |");
        let texts = texts(&lines, &document);
        assert_eq!(texts[2], "│ x|y │");
        assert_eq!(texts[0], "│ a   │");
        // Outside tables code is shown verbatim.
        let (lines, document) = doc("`x\\|y`");
        assert_eq!(rendered(&lines, &document, 0), "x\\|y");
    }

    #[test]
    fn incremental_reparse_is_local() {
        let mut lines: Vec<String> = Vec::new();
        for index in 0..20_000 {
            lines.push(format!("Paragraph {index} with **bold** text."));
            lines.push(String::new());
        }
        let mut document = Document::parse(&lines);
        lines[20_000] = "Edited *line*".to_owned();
        let changed = document.update(
            &lines,
            Damage {
                start: 20_000,
                old_end: 20_001,
                new_end: 20_001,
            },
        );
        assert!(
            changed.start >= 19_990 && changed.old_end <= 20_010,
            "{changed:?}"
        );
        assert_eq!(document, Document::parse(&lines));
    }
}
