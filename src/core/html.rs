//! html.rs
//! core::html
//!
//! Dependency-free HTML tree builder plus the text / markdown / links / readability converters.
//! No build script and no third-party parser crate (closed-network antivirus constraint).
//!

use std::collections::HashSet;

const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];
const SKIP_TAGS: &[&str] = &[
    "head", "script", "style", "noscript", "template", "svg", "math", "iframe", "object", "canvas",
    "select", "datalist", "title",
];
const P_CLOSERS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "details",
    "div",
    "dl",
    "fieldset",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "ul",
];
const BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "body",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "header",
    "html",
    "li",
    "main",
    "nav",
    "section",
    "summary",
    "tr",
];
const PRUNE_TAGS: &[&str] = &[
    "nav", "header", "footer", "aside", "form", "button", "dialog", "menu", "menuitem",
];
// class·id 토큰 비교 (4자 이상은 접두 일치)
const UNLIKELY_HINTS: &[&str] = &[
    "ad",
    "ads",
    "advert",
    "banner",
    "breadcrumb",
    "comment",
    "cookie",
    "footer",
    "gdpr",
    "masthead",
    "menu",
    "modal",
    "nav",
    "navbar",
    "navigation",
    "popup",
    "promo",
    "related",
    "share",
    "sidebar",
    "social",
    "sponsor",
    "subscribe",
    "toolbar",
    "widget",
];
const LIKELY_HINTS: &[&str] = &[
    "article", "body", "content", "entry", "main", "post", "story", "text",
];

// 1. DOM tree -----------------------------------------------------------------
#[derive(Debug)]
enum NodeKind {
    Element {
        name: String,
        attrs: Vec<(String, String)>,
    },
    Text(String),
}
#[derive(Debug)]
struct Node {
    kind: NodeKind,
    children: Vec<usize>,
}
// nodes[0] 은 가상 루트, 노드 순서 = 문서 순서
#[derive(Debug)]
struct Document {
    nodes: Vec<Node>,
}

// 2. 트리 구성 ----------------------------------------------------------------
fn parse_document(html: &str) -> Document {
    let mut nodes = vec![Node {
        kind: NodeKind::Element {
            name: "#root".to_string(),
            attrs: Vec::new(),
        },
        children: Vec::new(),
    }];
    let mut stack: Vec<usize> = vec![0];
    let mut text_start = 0usize;
    let mut cursor = 0usize;
    let push_text_node = |nodes: &mut Vec<Node>, parent: usize, raw: &str| {
        if raw.is_empty() {
            return;
        }
        let node_id = nodes.len();
        nodes.push(Node {
            kind: NodeKind::Text(decode_entities(raw)),
            children: Vec::new(),
        });
        nodes[parent].children.push(node_id);
    };

    while let Some(offset) = html[cursor..].find('<') {
        let lt = cursor + offset;
        let after = &html[lt + 1..];

        // 1. 주석·doctype·처리 지시문
        if after.starts_with('!') || after.starts_with('?') {
            let end = if let Some(comment) = after.strip_prefix("!--") {
                comment
                    .find("-->")
                    .map_or(html.len(), |index| lt + 4 + index + 3)
            } else {
                after
                    .find('>')
                    .map_or(html.len(), |index| lt + 1 + index + 1)
            };
            push_text_node(&mut nodes, stack[stack.len() - 1], &html[text_start..lt]);
            cursor = end;
            text_start = end;
            continue;
        }

        // 2. 끝 태그
        if let Some(rest) = after.strip_prefix('/') {
            let name_len = rest
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == ':'))
                .unwrap_or(rest.len());
            if name_len == 0 || !rest.as_bytes()[0].is_ascii_alphabetic() {
                cursor = lt + 1;
                continue;
            }
            push_text_node(&mut nodes, stack[stack.len() - 1], &html[text_start..lt]);
            let name = rest[..name_len].to_ascii_lowercase();
            let open_at = stack.iter().rposition(|&id| {
                matches!(&nodes[id].kind, NodeKind::Element { name: open, .. } if *open == name)
            });
            if let Some(index) = open_at
                && index > 0
            {
                stack.truncate(index);
            }
            let end = after
                .find('>')
                .map_or(html.len(), |index| lt + 1 + index + 1);
            cursor = end;
            text_start = end;
            continue;
        }

        // 3. 시작 태그
        let Some(StartTag {
            name,
            attrs,
            self_closing,
            end: tag_end,
        }) = parse_start_tag(html, lt)
        else {
            cursor = lt + 1;
            continue;
        };
        push_text_node(&mut nodes, stack[stack.len() - 1], &html[text_start..lt]);

        // 3-1. 암시적 닫힘
        let top_is_p = matches!(
            &nodes[stack[stack.len() - 1]].kind,
            NodeKind::Element { name: open, .. } if open == "p"
        );
        if P_CLOSERS.contains(&name.as_str()) && top_is_p {
            stack.pop();
        }
        let implied: Option<(&[&str], &[&str])> = match name.as_str() {
            "li" => Some((&["li"], &["ul", "ol", "menu"])),
            "dt" | "dd" => Some((&["dt", "dd"], &["dl"])),
            "tr" => Some((&["tr", "td", "th"], &["table", "thead", "tbody", "tfoot"])),
            "td" | "th" => Some((&["td", "th"], &["tr", "table"])),
            "thead" | "tbody" | "tfoot" => {
                Some((&["thead", "tbody", "tfoot", "tr", "td", "th"], &["table"]))
            }
            "option" => Some((&["option"], &["select", "datalist", "optgroup"])),
            _ => None,
        };
        if let Some((targets, boundaries)) = implied {
            for index in (1..stack.len()).rev() {
                let NodeKind::Element { name: open, .. } = &nodes[stack[index]].kind else {
                    continue;
                };
                if boundaries.contains(&open.as_str()) {
                    break;
                }
                if targets.contains(&open.as_str()) {
                    stack.truncate(index);
                    break;
                }
            }
        }

        // 3-2. 노드 추가
        let attrs = attrs
            .into_iter()
            .map(|(attr_name, raw_value)| (attr_name, decode_entities(&raw_value)))
            .collect();
        let node_id = nodes.len();
        nodes.push(Node {
            kind: NodeKind::Element {
                name: name.clone(),
                attrs,
            },
            children: Vec::new(),
        });
        let parent = stack[stack.len() - 1];
        nodes[parent].children.push(node_id);
        cursor = tag_end;
        text_start = tag_end;
        if self_closing || VOID_TAGS.contains(&name.as_str()) {
            continue;
        }

        // 3-3. 원문 텍스트 요소
        let raw_text = matches!(
            name.as_str(),
            "script" | "style" | "textarea" | "title" | "xmp"
        );
        if !raw_text {
            stack.push(node_id);
            continue;
        }
        let closing = format!("</{name}");
        let body_end = html.as_bytes()[tag_end..]
            .windows(closing.len())
            .position(|window| window.eq_ignore_ascii_case(closing.as_bytes()))
            .map_or(html.len(), |index| tag_end + index);
        if matches!(name.as_str(), "textarea" | "title" | "xmp") {
            push_text_node(&mut nodes, node_id, &html[tag_end..body_end]);
        }
        let end = html[body_end..]
            .find('>')
            .map_or(html.len(), |index| body_end + index + 1);
        cursor = end;
        text_start = end;
    }
    push_text_node(&mut nodes, stack[stack.len() - 1], &html[text_start..]);
    Document { nodes }
}
// 문자 참조 해석 (숫자·주요 이름)
fn decode_entities(raw: &str) -> String {
    let mut text = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        text.push_str(&rest[..amp]);
        let tail = &rest[amp + 1..];
        let end = tail
            .char_indices()
            .take(32)
            .find(|&(_, ch)| ch == ';' || !(ch.is_ascii_alphanumeric() || ch == '#'))
            .filter(|&(_, ch)| ch == ';');
        let Some((semi, _)) = end else {
            text.push('&');
            rest = tail;
            continue;
        };
        let entity = &tail[..semi];
        let decoded = if let Some(number) = entity.strip_prefix('#') {
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => number.parse::<u32>().ok(),
            };
            code.and_then(char::from_u32)
        } else {
            match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                "copy" => Some('\u{a9}'),
                "reg" => Some('\u{ae}'),
                "trade" => Some('\u{2122}'),
                "hellip" => Some('\u{2026}'),
                "mdash" => Some('\u{2014}'),
                "ndash" => Some('\u{2013}'),
                "lsquo" => Some('\u{2018}'),
                "rsquo" => Some('\u{2019}'),
                "ldquo" => Some('\u{201c}'),
                "rdquo" => Some('\u{201d}'),
                "laquo" => Some('\u{ab}'),
                "raquo" => Some('\u{bb}'),
                "middot" => Some('\u{b7}'),
                "bull" => Some('\u{2022}'),
                "times" => Some('\u{d7}'),
                "deg" => Some('\u{b0}'),
                "euro" => Some('\u{20ac}'),
                "yen" => Some('\u{a5}'),
                "pound" => Some('\u{a3}'),
                "cent" => Some('\u{a2}'),
                "sect" => Some('\u{a7}'),
                "para" => Some('\u{b6}'),
                "larr" => Some('\u{2190}'),
                "rarr" => Some('\u{2192}'),
                "uarr" => Some('\u{2191}'),
                "darr" => Some('\u{2193}'),
                "zwj" => Some('\u{200d}'),
                "zwnj" => Some('\u{200c}'),
                _ => None,
            }
        };
        match decoded {
            Some(ch) => text.push(ch),
            None => {
                text.push('&');
                text.push_str(entity);
                text.push(';');
            }
        }
        rest = &tail[semi + 1..];
    }
    text.push_str(rest);
    text
}
// 시작 태그 파싱 결과 (속성은 원문)
struct StartTag {
    name: String,
    attrs: Vec<(String, String)>,
    self_closing: bool,
    end: usize,
}
fn parse_start_tag(html: &str, lt: usize) -> Option<StartTag> {
    let bytes = html.as_bytes();
    let mut pos = lt + 1;
    if pos >= bytes.len() || !bytes[pos].is_ascii_alphabetic() {
        return None;
    }
    let name_start = pos;
    while pos < bytes.len()
        && !bytes[pos].is_ascii_whitespace()
        && bytes[pos] != b'>'
        && bytes[pos] != b'/'
    {
        pos += 1;
    }
    let name = html[name_start..pos].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut self_closing = false;

    loop {
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() {
            return Some(StartTag {
                name,
                attrs,
                self_closing,
                end: bytes.len(),
            });
        }
        match bytes[pos] {
            b'>' => {
                return Some(StartTag {
                    name,
                    attrs,
                    self_closing,
                    end: pos + 1,
                });
            }
            b'/' => {
                self_closing = true;
                pos += 1;
                continue;
            }
            _ => self_closing = false,
        }

        // 속성 이름
        let attr_start = pos;
        while pos < bytes.len()
            && !bytes[pos].is_ascii_whitespace()
            && !matches!(bytes[pos], b'>' | b'=' | b'/')
        {
            pos += 1;
        }
        if pos == attr_start {
            pos += 1;
            continue;
        }
        let attr_name = html[attr_start..pos].to_ascii_lowercase();
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() || bytes[pos] != b'=' {
            attrs.push((attr_name, String::new()));
            continue;
        }

        // 속성 값
        pos += 1;
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let raw_value = if pos < bytes.len() && matches!(bytes[pos], b'"' | b'\'') {
            let quote = bytes[pos];
            let value_start = pos + 1;
            let value_end = bytes[value_start..]
                .iter()
                .position(|&byte| byte == quote)
                .map_or(bytes.len(), |index| value_start + index);
            pos = (value_end + 1).min(bytes.len());
            &html[value_start..value_end]
        } else {
            let value_start = pos;
            while pos < bytes.len() && !bytes[pos].is_ascii_whitespace() && bytes[pos] != b'>' {
                pos += 1;
            }
            &html[value_start..pos]
        };
        attrs.push((attr_name, raw_value.to_string()));
    }
}

// 3. 블록 출력기 --------------------------------------------------------------
struct BlockWriter<'a> {
    doc: &'a Document,
    skipped: Option<&'a HashSet<usize>>,
    markdown: bool,
    out: String,
    prefix: String,
    pending_breaks: usize,
    pending_space: bool,
    pre_depth: usize,
    list_stack: Vec<Option<usize>>,
    // 첫 내용 앞 (바깥 접두어, 글머리표)
    pending_marker: Option<(String, String)>,
}
impl<'a> BlockWriter<'a> {
    fn new(doc: &'a Document, skipped: Option<&'a HashSet<usize>>, markdown: bool) -> Self {
        Self {
            doc,
            skipped,
            markdown,
            out: String::new(),
            prefix: String::new(),
            pending_breaks: 0,
            pending_space: false,
            pre_depth: 0,
            list_stack: Vec::new(),
            pending_marker: None,
        }
    }
    // 대기 중인 줄바꿈·공백을 반영한 뒤 조각 기록
    fn emit_piece(&mut self, piece: &str) {
        if piece.is_empty() {
            return;
        }
        let line_prefix = match self.pending_marker.take() {
            Some((outer, marker)) => {
                self.pending_breaks = self.pending_breaks.max(1);
                format!("{outer}{marker}")
            }
            None => self.prefix.clone(),
        };
        if self.out.is_empty() {
            self.out.push_str(&line_prefix);
        } else if self.pending_breaks > 0 {
            let trimmed = self.out.trim_end_matches(' ').len();
            self.out.truncate(trimmed);
            for _ in 0..self.pending_breaks {
                self.out.push('\n');
            }
            self.out.push_str(&line_prefix);
        } else if self.pending_space && !self.out.ends_with([' ', '\n']) {
            self.out.push(' ');
        }
        self.pending_breaks = 0;
        self.pending_space = false;
        self.out.push_str(piece);
    }
    // 공백 축약 텍스트, pre 안에서는 원문 유지
    fn emit_text(&mut self, text: &str) {
        if self.pre_depth > 0 {
            for (index, line) in text.split('\n').enumerate() {
                if index > 0 {
                    self.pending_breaks += 1;
                }
                self.emit_piece(line.trim_end_matches('\r'));
            }
            return;
        }
        let mut word_start: Option<usize> = None;
        for (index, ch) in text.char_indices() {
            let is_space = ch.is_whitespace() && ch != '\u{a0}';
            match (is_space, word_start) {
                (true, Some(start)) => {
                    self.emit_piece(&text[start..index]);
                    self.pending_space = true;
                    word_start = None;
                }
                (true, None) => self.pending_space = true,
                (false, None) => word_start = Some(index),
                (false, Some(_)) => {}
            }
        }
        if let Some(start) = word_start {
            self.emit_piece(&text[start..]);
        }
    }
    fn write_children(&mut self, id: usize) {
        for &child in &self.doc.nodes[id].children {
            self.write_node(child);
        }
    }
    fn write_node(&mut self, id: usize) {
        if self.skipped.is_some_and(|skipped| skipped.contains(&id)) {
            return;
        }
        let (name, attrs) = match &self.doc.nodes[id].kind {
            NodeKind::Text(text) => {
                self.emit_text(text);
                return;
            }
            NodeKind::Element { name, attrs } => (name.as_str(), attrs),
        };
        let attr = |key: &str| {
            attrs
                .iter()
                .find(|(attr_name, _)| attr_name == key)
                .map_or("", |(_, attr_value)| attr_value.trim())
        };
        if SKIP_TAGS.contains(&name) {
            return;
        }
        match name {
            // 1. 제목
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.pending_breaks = self.pending_breaks.max(2);
                if self.markdown {
                    let level = usize::from(name.as_bytes()[1] - b'0');
                    self.emit_piece(&format!("{} ", "#".repeat(level)));
                }
                self.write_children(id);
                self.pending_breaks = self.pending_breaks.max(2);
            }

            // 2. 문단·구획
            "p" => {
                self.pending_breaks = self.pending_breaks.max(2);
                self.write_children(id);
                self.pending_breaks = self.pending_breaks.max(2);
            }
            "br" => self.pending_breaks = (self.pending_breaks + 1).min(2),
            "hr" => {
                self.pending_breaks = self.pending_breaks.max(2);
                self.emit_piece("---");
                self.pending_breaks = self.pending_breaks.max(2);
            }
            "blockquote" => {
                self.pending_breaks = self.pending_breaks.max(2);
                let saved = self.prefix.clone();
                self.prefix.push_str("> ");
                self.write_children(id);
                self.prefix = saved;
                self.pending_breaks = self.pending_breaks.max(2);
            }

            // 3. 코드 블록
            "pre" => {
                self.pending_breaks = self.pending_breaks.max(2);
                if self.markdown {
                    self.emit_piece("```");
                    self.pending_breaks = 1;
                }
                self.pre_depth += 1;
                let first_text = self.doc.nodes[id].children.first().copied();
                for &child in &self.doc.nodes[id].children {
                    match &self.doc.nodes[child].kind {
                        NodeKind::Text(text) if Some(child) == first_text => {
                            self.emit_text(text.strip_prefix('\n').unwrap_or(text));
                        }
                        _ => self.write_node(child),
                    }
                }
                self.pre_depth -= 1;
                if self.markdown {
                    self.pending_breaks = 1;
                    self.emit_piece("```");
                }
                self.pending_breaks = self.pending_breaks.max(2);
            }

            // 4. 목록
            "ul" | "ol" | "menu" => {
                let nested = !self.list_stack.is_empty();
                self.pending_breaks = self.pending_breaks.max(if nested { 1 } else { 2 });
                let start = attr("start").parse::<usize>().unwrap_or(1);
                self.list_stack.push((name == "ol").then_some(start));
                self.write_children(id);
                self.list_stack.pop();
                self.pending_breaks = self.pending_breaks.max(if nested { 1 } else { 2 });
            }
            "li" => {
                self.pending_breaks = self.pending_breaks.max(1);
                let marker = match self.list_stack.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "* ".to_string(),
                };
                let saved = self.prefix.clone();
                self.prefix.push_str(&" ".repeat(marker.len()));
                self.pending_marker = Some((saved.clone(), marker));
                self.write_children(id);
                self.pending_marker = None;
                self.prefix = saved;
                self.pending_breaks = self.pending_breaks.max(1);
            }

            // 5. 표
            "table" => {
                self.pending_breaks = self.pending_breaks.max(2);
                let mut rows: Vec<usize> = Vec::new();
                let mut queue: Vec<usize> =
                    self.doc.nodes[id].children.iter().rev().copied().collect();
                while let Some(node) = queue.pop() {
                    let NodeKind::Element {
                        name: child_name, ..
                    } = &self.doc.nodes[node].kind
                    else {
                        continue;
                    };
                    match child_name.as_str() {
                        "tr" => rows.push(node),
                        "thead" | "tbody" | "tfoot" => {
                            queue.extend(self.doc.nodes[node].children.iter().rev().copied());
                        }
                        _ => {}
                    }
                }
                for (row_index, &row) in rows.iter().enumerate() {
                    let mut cells: Vec<String> = Vec::new();
                    for &cell in &self.doc.nodes[row].children {
                        let is_cell = matches!(
                            &self.doc.nodes[cell].kind,
                            NodeKind::Element { name: cell_name, .. } if cell_name == "td" || cell_name == "th"
                        );
                        if !is_cell {
                            continue;
                        }
                        let mut cell_writer =
                            BlockWriter::new(self.doc, self.skipped, self.markdown);
                        cell_writer.write_children(cell);
                        let cell_text = cell_writer.finish_output().replace('\n', " ");
                        cells.push(if self.markdown {
                            cell_text.replace('|', "\\|")
                        } else {
                            cell_text
                        });
                    }
                    if cells.is_empty() {
                        continue;
                    }
                    self.pending_breaks = self.pending_breaks.max(1);
                    if !self.markdown {
                        self.emit_piece(&cells.join(" | "));
                        continue;
                    }
                    self.emit_piece(&format!("| {} |", cells.join(" | ")));
                    if row_index == 0 {
                        self.pending_breaks = 1;
                        self.emit_piece(&format!("|{}", " --- |".repeat(cells.len())));
                    }
                }
                self.pending_breaks = self.pending_breaks.max(2);
            }

            // 6. 인라인 강조·코드
            "strong" | "b" | "em" | "i" | "del" | "s" | "code" | "kbd" | "samp"
                if self.markdown && self.pre_depth == 0 =>
            {
                let mark = match name {
                    "strong" | "b" => "**",
                    "em" | "i" => "*",
                    "del" | "s" => "~~",
                    _ => "`",
                };
                self.emit_piece(mark);
                let before = self.out.len();
                self.write_children(id);
                if self.out.len() == before {
                    let trimmed = before - mark.len();
                    self.out.truncate(trimmed);
                } else {
                    self.out.push_str(mark);
                }
            }

            // 7. 링크·이미지
            "a" if self.markdown => {
                let href = attr("href");
                let linkable = !href.is_empty()
                    && !href.starts_with('#')
                    && !href.to_ascii_lowercase().starts_with("javascript:");
                if !linkable {
                    self.write_children(id);
                    return;
                }
                self.emit_piece("[");
                self.write_children(id);
                self.out.push_str(&format!("]({href})"));
            }
            "img" => {
                let alt = attr("alt");
                let src = attr("src");
                if self.markdown && !src.is_empty() {
                    self.emit_piece(&format!("![{alt}]({src})"));
                } else if !alt.is_empty() {
                    self.emit_piece(alt);
                }
            }

            // 8. 기타 블록·인라인
            _ if BLOCK_TAGS.contains(&name) => {
                self.pending_breaks = self.pending_breaks.max(1);
                self.write_children(id);
                self.pending_breaks = self.pending_breaks.max(1);
            }
            _ => self.write_children(id),
        }
    }
    fn finish_output(self) -> String {
        let trimmed: Vec<&str> = self.out.lines().map(str::trim_end).collect();
        trimmed.join("\n").trim_matches('\n').to_string()
    }
}

// 4. 공개 변환 ----------------------------------------------------------------
pub fn html_to_text(html: &str) -> String {
    let doc = parse_document(html);
    let mut writer = BlockWriter::new(&doc, None, false);
    writer.write_children(0);
    writer.finish_output()
}
pub fn html_to_markdown(html: &str) -> String {
    let doc = parse_document(html);
    let mut writer = BlockWriter::new(&doc, None, true);
    writer.write_children(0);
    writer.finish_output()
}
pub fn html_links(html: &str, base: Option<&str>) -> Vec<String> {
    let doc = parse_document(html);
    let mut seen = HashSet::new();
    let mut links = Vec::new();
    for node in &doc.nodes {
        let NodeKind::Element { name, attrs } = &node.kind else {
            continue;
        };
        if name != "a" {
            continue;
        }
        let Some((_, href)) = attrs.iter().find(|(attr_name, _)| attr_name == "href") else {
            continue;
        };
        let href = href.trim();
        if href.is_empty()
            || href.starts_with('#')
            || href.starts_with("javascript:")
            || href.starts_with("mailto:")
            || href.starts_with("tel:")
        {
            continue;
        }
        let resolved = match base {
            Some(base) => crate::core::web::resolve_url(base, href),
            None => href.to_string(),
        };
        if seen.insert(resolved.clone()) {
            links.push(resolved);
        }
    }
    links
}

// 5. 본문 추출 ----------------------------------------------------------------
#[derive(Clone, Debug)]
pub struct ReadableDoc {
    pub title: String,
    pub text: String,
    pub content_html: String,
    pub byline: String,
    pub length: usize,
}
pub fn html_readability(html: &str) -> ReadableDoc {
    let doc = parse_document(html);
    let element = |id: usize| match &doc.nodes[id].kind {
        NodeKind::Element { name, attrs } => Some((name.as_str(), attrs)),
        NodeKind::Text(_) => None,
    };
    let attr_of = |attrs: &Vec<(String, String)>, key: &str| -> String {
        attrs
            .iter()
            .find(|(attr_name, _)| attr_name == key)
            .map_or(String::new(), |(_, attr_value)| {
                attr_value.trim().to_string()
            })
    };

    // 1. 부모 연결·잡음 요소 표시
    let mut parent = vec![0usize; doc.nodes.len()];
    let mut skipped: HashSet<usize> = HashSet::new();
    for id in 0..doc.nodes.len() {
        for &child in &doc.nodes[id].children {
            parent[child] = id;
        }
        let Some((name, attrs)) = element(id) else {
            continue;
        };
        let hint =
            format!("{} {}", attr_of(attrs, "class"), attr_of(attrs, "id")).to_ascii_lowercase();
        let tokens: Vec<&str> = hint
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
            .collect();
        let hits = |words: &[&str]| {
            tokens.iter().any(|token| {
                words
                    .iter()
                    .any(|word| *token == *word || (word.len() >= 4 && token.starts_with(word)))
            })
        };
        let unlikely = hits(UNLIKELY_HINTS) && !hits(LIKELY_HINTS);
        let hidden = attrs.iter().any(|(attr_name, _)| attr_name == "hidden")
            || attr_of(attrs, "aria-hidden") == "true";
        if PRUNE_TAGS.contains(&name) || SKIP_TAGS.contains(&name) || unlikely || hidden {
            skipped.insert(id);
        }
    }

    // 2. 제목·작성자
    let mut title = String::new();
    let mut og_title = String::new();
    let mut first_h1 = String::new();
    let mut byline = String::new();
    for id in 0..doc.nodes.len() {
        let Some((name, attrs)) = element(id) else {
            continue;
        };
        match name {
            "title" if title.is_empty() => {
                let mut writer = BlockWriter::new(&doc, None, false);
                writer.write_children(id);
                title = writer.finish_output().replace('\n', " ");
            }
            "h1" if first_h1.is_empty() => {
                let mut writer = BlockWriter::new(&doc, None, false);
                writer.write_children(id);
                first_h1 = writer.finish_output().replace('\n', " ");
            }
            "meta" => {
                let key = format!("{}{}", attr_of(attrs, "name"), attr_of(attrs, "property"))
                    .to_ascii_lowercase();
                if key == "og:title" && og_title.is_empty() {
                    og_title = attr_of(attrs, "content");
                }
                if (key == "author" || key == "article:author") && byline.is_empty() {
                    byline = attr_of(attrs, "content");
                }
            }
            _ => {}
        }
    }
    if title.is_empty() {
        title = if og_title.is_empty() {
            first_h1
        } else {
            og_title
        };
    }

    // 3. 본문 후보 점수
    let mut scores = vec![0.0f64; doc.nodes.len()];
    let mut best_root: Option<usize> = None;
    for id in 0..doc.nodes.len() {
        let Some((name, attrs)) = element(id) else {
            continue;
        };
        let mut ancestor = id;
        let mut inside_skipped = false;
        while ancestor != 0 {
            if skipped.contains(&ancestor) {
                inside_skipped = true;
                break;
            }
            ancestor = parent[ancestor];
        }
        if inside_skipped {
            continue;
        }
        if best_root.is_none()
            && (name == "article" || name == "main" || attr_of(attrs, "role") == "main")
        {
            let mut writer = BlockWriter::new(&doc, Some(&skipped), false);
            writer.write_children(id);
            if writer.finish_output().chars().count() >= 200 {
                best_root = Some(id);
            }
        }
        if !matches!(name, "p" | "pre" | "td" | "blockquote") {
            continue;
        }
        let mut writer = BlockWriter::new(&doc, Some(&skipped), false);
        writer.write_children(id);
        let paragraph = writer.finish_output();
        let length = paragraph.chars().count();
        if length < 25 {
            continue;
        }
        let commas = paragraph.matches([',', '，', '、']).count() as f64;
        let score = 1.0 + commas + (length as f64 / 100.0).min(3.0);
        let first = parent[id];
        scores[first] += score;
        if first != 0 {
            scores[parent[first]] += score / 2.0;
        }
    }
    let root = best_root.unwrap_or_else(|| {
        let mut best = 0usize;
        for (id, &score) in scores.iter().enumerate().skip(1) {
            if score > scores[best] {
                best = id;
            }
        }
        best
    });

    // 4. 본문 텍스트·HTML
    let mut writer = BlockWriter::new(&doc, Some(&skipped), false);
    writer.write_node(root);
    let text = writer.finish_output();
    let mut content_html = String::new();
    let mut work: Vec<(usize, bool)> = vec![(root, false)];
    while let Some((id, closing)) = work.pop() {
        if skipped.contains(&id) && id != root {
            continue;
        }
        match &doc.nodes[id].kind {
            NodeKind::Text(text_node) => {
                content_html.push_str(
                    &text_node
                        .replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;"),
                );
            }
            NodeKind::Element { name, .. } if name == "#root" => {
                work.extend(
                    doc.nodes[id]
                        .children
                        .iter()
                        .rev()
                        .map(|&child| (child, false)),
                );
            }
            NodeKind::Element { name, .. } if closing => {
                content_html.push_str(&format!("</{name}>"));
            }
            NodeKind::Element { name, attrs } => {
                content_html.push('<');
                content_html.push_str(name);
                for (attr_name, attr_value) in attrs {
                    let escaped = attr_value.replace('&', "&amp;").replace('"', "&quot;");
                    content_html.push_str(&format!(" {attr_name}=\"{escaped}\""));
                }
                content_html.push('>');
                if VOID_TAGS.contains(&name.as_str()) {
                    continue;
                }
                work.push((id, true));
                work.extend(
                    doc.nodes[id]
                        .children
                        .iter()
                        .rev()
                        .map(|&child| (child, false)),
                );
            }
        }
    }
    let length = text.chars().count();
    ReadableDoc {
        title,
        text,
        content_html,
        byline,
        length,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1. 링크 추출 ------------------------------------------------------------
    #[test]
    fn extracts_and_dedupes_links() {
        let html = "<a href=\"/x\">1</a><a href=\"/x\">dup</a><a href=\"#f\">frag</a><a href=\"https://e.com/y\">2</a>";
        let links = html_links(html, Some("https://ex.com/base"));
        assert_eq!(links, vec!["https://ex.com/x", "https://e.com/y"]);
    }
    // 2-1. 텍스트·마크다운 변환 -----------------------------------------------
    #[test]
    fn converts_markdown_and_text() {
        let html = "<h1>Title</h1><p>Body</p>";
        assert_eq!(html_to_markdown(html), "# Title\n\nBody");
        assert_eq!(html_to_text(html), "Title\n\nBody");
    }
    // 2-2. 텍스트·마크다운 변환 -----------------------------------------------
    #[test]
    fn skips_scripts_and_decodes_entities() {
        let html = "<html><head><title>T</title><style>p{}</style></head><body><script>if (a < b) {}</script><p>A &amp; B &lt;3 &#x41;&#66; &unknown; R&D</p></body></html>";
        assert_eq!(html_to_text(html), "A & B <3 AB &unknown; R&D");
    }
    // 2-3. 텍스트·마크다운 변환 -----------------------------------------------
    #[test]
    fn renders_lists_inline_and_code() {
        let html = "<p>see <a href=\"/doc\">the <b>docs</b></a></p><ul><li>one<li>two<ol><li>nested</ol></ul><pre>\nfn main() {\n    x\n}</pre>";
        let markdown = html_to_markdown(html);
        assert_eq!(
            markdown,
            "see [the **docs**](/doc)\n\n* one\n* two\n  1. nested\n\n```\nfn main() {\n    x\n}\n```"
        );
    }
    // 2-4. 텍스트·마크다운 변환 -----------------------------------------------
    #[test]
    fn keeps_marker_on_block_item() {
        let html = "<ul><li><div>A</div><div>B</div></li><li><button><span>C</span></button></li><li></li></ul>";
        assert_eq!(html_to_text(html), "* A\n  B\n* C");
    }
    // 2-5. 텍스트·마크다운 변환 -----------------------------------------------
    #[test]
    fn renders_tables() {
        let html = "<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>";
        assert_eq!(
            html_to_markdown(html),
            "| A | B |\n| --- | --- |\n| 1 | 2 |"
        );
        assert_eq!(html_to_text(html), "A | B\n1 | 2");
    }
    // 3. 깨진 마크업 ----------------------------------------------------------
    #[test]
    fn tolerates_broken_markup() {
        let html = "<p>one<p>two</span><div>three < four</div><p unterminated=\"x";
        assert_eq!(html_to_text(html), "one\n\ntwo\n\nthree < four");
    }
    // 4. 본문 추출 ------------------------------------------------------------
    #[test]
    fn extracts_readable_article() {
        let body = "Long article sentence, with commas, that keeps going for a while. ".repeat(6);
        let html = format!(
            "<html><head><title>Page</title><meta name=\"author\" content=\"Kim\"></head><body><nav><a href=\"/\">Home</a></nav><div class=\"sidebar\"><p>{body}</p></div><div id=\"content\"><p>{body}</p><p>{body}</p></div><footer>foot</footer></body></html>"
        );
        let doc = html_readability(&html);
        assert_eq!(doc.title, "Page");
        assert_eq!(doc.byline, "Kim");
        assert!(doc.text.starts_with("Long article"), "{}", doc.text);
        assert!(!doc.text.contains("Home"));
        assert!(!doc.text.contains("foot"));
        assert!(doc.content_html.starts_with("<div id=\"content\">"));
        assert_eq!(doc.length, doc.text.chars().count());
    }
}
