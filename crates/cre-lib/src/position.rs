use anyhow::{Result, anyhow};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PathSegment {
    pub tag: String,
    pub index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPosition {
    pub spine_index: u32,
    pub path: Vec<PathSegment>,
    /// 1-based index among the parent's text nodes, when there is more than
    /// one. crengine emits `/text()[k]` for this; without it a position inside
    /// one paragraph is indistinguishable from another position in the same
    /// paragraph.
    pub text_index: Option<u32>,
    pub codepoint_offset: u32,
    /// The path names an element rather than a character inside its text.
    ///
    /// A page that holds only a picture has no text to anchor to, so crengine
    /// points at the element instead (`/body/DocFragment[2]/body/div/img`).
    /// `text_index` and `codepoint_offset` are unused then.
    pub element: bool,
}

impl TextPosition {
    pub fn to_xpointer(&self) -> String {
        let mut path = String::new();
        for segment in &self.path {
            path.push('/');
            path.push_str(&segment.tag);
            path.push('[');
            path.push_str(&segment.index.to_string());
            path.push(']');
        }

        if self.element {
            return format!("/body/DocFragment[{}]/body{}", self.spine_index, path);
        }

        let text = match self.text_index {
            Some(index) => format!("text()[{index}]"),
            None => "text()".to_string(),
        };
        format!(
            "/body/DocFragment[{}]/body{}/{}.{}",
            self.spine_index, path, text, self.codepoint_offset
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XPointerRange {
    pub start: TextPosition,
    pub end: Option<TextPosition>,
    pub id: Option<String>,
}

impl XPointerRange {
    pub fn point(start: TextPosition) -> Self {
        Self {
            start,
            end: None,
            id: None,
        }
    }

    pub fn to_xpointer(&self) -> String {
        match self.end.as_ref() {
            Some(end) => {
                let id = self.id.as_deref().unwrap_or("");
                if id.is_empty() {
                    format!("range({},{})", self.start.to_xpointer(), end.to_xpointer())
                } else {
                    format!(
                        "range({},{}) with-id({})",
                        self.start.to_xpointer(),
                        end.to_xpointer(),
                        id
                    )
                }
            }
            None => self.start.to_xpointer(),
        }
    }

    pub fn parse(input: &str) -> Result<Self> {
        Ok(Self {
            start: parse_text_position(input)?,
            end: None,
            id: None,
        })
    }
}

pub fn parse_text_position(input: &str) -> Result<TextPosition> {
    let input = input.trim();
    let path = if let Some(rest) = input.strip_prefix("xpointer(") {
        rest.strip_suffix(')')
            .ok_or_else(|| anyhow!("unsupported XPointer: xpointer() scheme is not closed"))?
    } else if input.starts_with("range(") {
        return Err(anyhow!(
            "unsupported XPointer: range() is not supported in this step"
        ));
    } else {
        input
    };

    if !path.starts_with('/') {
        return Err(anyhow!("unsupported XPointer: path must start with /"));
    }

    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 4 || !parts[0].is_empty() || parts[1] != "body" {
        return Err(anyhow!(
            "unsupported XPointer: expected /body/DocFragment[n]/body"
        ));
    }

    let spine_index = parse_docfragment(parts[2])?;
    let mut cursor = 3;
    if parts[cursor] == "body" {
        cursor += 1;
    }

    let Some(terminal) = parts.last() else {
        return Err(anyhow!("unsupported XPointer: path is empty"));
    };
    // A terminal `text()[k].N` is a character; anything else is an element
    // (crengine uses that form for a position with no text, such as a picture).
    let (text_index, codepoint_offset, element) = match parse_terminal(terminal) {
        Ok((text_index, codepoint_offset)) => (text_index, codepoint_offset, false),
        Err(_) => (None, 0, true),
    };

    let end = if element {
        parts.len()
    } else {
        parts.len() - 1
    };
    let mut segments = Vec::new();
    for part in parts.iter().take(end).skip(cursor) {
        segments.push(parse_path_segment(part)?);
    }
    if element && segments.is_empty() {
        return Err(anyhow!("unsupported XPointer: element path is empty"));
    }

    Ok(TextPosition {
        spine_index,
        path: segments,
        text_index,
        codepoint_offset,
        element,
    })
}

fn parse_docfragment(segment: &str) -> Result<u32> {
    let rest = segment
        .strip_prefix("DocFragment[")
        .ok_or_else(|| anyhow!("unsupported XPointer: missing DocFragment[n]"))?;
    let value = rest
        .strip_suffix(']')
        .ok_or_else(|| anyhow!("unsupported XPointer: malformed DocFragment[n]"))?;
    let index = value
        .parse::<u32>()
        .map_err(|_| anyhow!("unsupported XPointer: DocFragment index must be positive"))?;
    if index == 0 {
        return Err(anyhow!(
            "unsupported XPointer: DocFragment index must be positive"
        ));
    }
    Ok(index)
}

fn parse_terminal(segment: &str) -> Result<(Option<u32>, u32)> {
    // Accepted forms: text().N, text()[k].N
    let (text_index, rest) = if let Some(rest) = segment.strip_prefix("text()[") {
        let (index, rest) = rest
            .split_once(']')
            .ok_or_else(|| anyhow!("unsupported XPointer: malformed text()[k]"))?;
        let index = index
            .parse::<u32>()
            .map_err(|_| anyhow!("unsupported XPointer: malformed text() index"))?;
        if index == 0 {
            return Err(anyhow!(
                "unsupported XPointer: text() index must be positive"
            ));
        }
        (Some(index), rest)
    } else if let Some(rest) = segment.strip_prefix("text()") {
        (None, rest)
    } else {
        return Err(anyhow!("unsupported XPointer: path must end with text().0"));
    };

    let rest = rest.strip_prefix('.').unwrap_or(rest);
    if rest.is_empty() {
        return Ok((text_index, 0));
    }
    let offset = rest
        .parse::<u32>()
        .map_err(|_| anyhow!("unsupported XPointer: malformed text() offset"))?;
    Ok((text_index, offset))
}

fn parse_path_segment(segment: &str) -> Result<PathSegment> {
    if segment.is_empty() || segment == "text()" {
        return Err(anyhow!("unsupported XPointer: malformed path segment"));
    }
    // A segment is a tag name, optionally with a 1-based index. Anything else
    // (a function call, an offset) is not part of a path.
    let tag_is_name = |tag: &str| {
        !tag.is_empty()
            && tag
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':'))
    };

    if let Some(open) = segment.find('[') {
        if !segment.ends_with(']') {
            return Err(anyhow!(
                "unsupported XPointer: malformed path segment index"
            ));
        }
        let tag = &segment[..open];
        let index = &segment[open + 1..segment.len() - 1];
        let index = index
            .parse::<u32>()
            .map_err(|_| anyhow!("unsupported XPointer: malformed path segment index"))?;
        if !tag_is_name(tag) || index == 0 {
            return Err(anyhow!(
                "unsupported XPointer: malformed path segment index"
            ));
        }
        Ok(PathSegment {
            tag: tag.to_string(),
            index,
        })
    } else {
        if !tag_is_name(segment) {
            return Err(anyhow!("unsupported XPointer: malformed path segment"));
        }
        Ok(PathSegment {
            tag: segment.to_string(),
            index: 1,
        })
    }
}
