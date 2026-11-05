//! A small modal form: text fields (optionally masked or multi-line through paste) and
//! choices, moved through with Tab / ↑↓, submitted with Enter. Production-looking
//! environments add a field where the environment name must be typed.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};

use super::theme;
use super::widgets::{self, InputEvent, TextInput};

pub enum Kind {
    Text,
    /// Masked until Ctrl+R; pasted line breaks are kept.
    Secret,
    Choice(&'static [&'static str]),
    /// Shown, not editable.
    Fixed,
}

pub struct Field {
    pub label: &'static str,
    pub input: TextInput,
    pub kind: Kind,
    choice: usize,
    required: bool,
    placeholder: &'static str,
    /// Masked for display (Secret fields start masked).
    pub masked: bool,
}

impl Field {
    pub fn text(label: &'static str, value: impl Into<String>) -> Field {
        Field {
            label,
            input: TextInput::with_value(value),
            kind: Kind::Text,
            choice: 0,
            required: true,
            placeholder: "",
            masked: false,
        }
    }

    pub fn secret(label: &'static str, value: impl Into<String>) -> Field {
        Field { kind: Kind::Secret, masked: true, ..Field::text(label, value) }
    }

    pub fn choice(label: &'static str, options: &'static [&'static str], selected: &str) -> Field {
        let choice = options.iter().position(|o| *o == selected).unwrap_or(0);
        Field { kind: Kind::Choice(options), choice, ..Field::text(label, "") }
    }

    pub fn fixed(label: &'static str, value: impl Into<String>) -> Field {
        Field { kind: Kind::Fixed, ..Field::text(label, value) }
    }

    /// Grey text shown while the field is empty.
    pub fn hint(mut self, placeholder: &'static str) -> Field {
        self.placeholder = placeholder;
        self
    }

    pub fn optional(mut self, placeholder: &'static str) -> Field {
        self.required = false;
        self.placeholder = placeholder;
        self
    }

    pub fn value(&self) -> &str {
        match self.kind {
            Kind::Choice(options) => options[self.choice],
            _ => &self.input.value,
        }
    }

    fn editable(&self) -> bool {
        !matches!(self.kind, Kind::Fixed)
    }
}

pub enum FormEvent {
    Pending,
    Cancel,
    Submit,
}

pub struct Form {
    pub title: String,
    pub fields: Vec<Field>,
    focus: usize,
    /// Environment name to type before submitting (production-looking environments).
    confirm: Option<(String, TextInput)>,
    error: Option<String>,
    /// Shown under the fields.
    pub note: Option<String>,
    /// What submitting does, for the hints.
    submit_label: &'static str,
    /// Enter submits from any field instead of moving to the next one.
    enter_submits: bool,
}

impl Form {
    pub fn new(title: impl Into<String>, fields: Vec<Field>) -> Form {
        let focus = fields.iter().position(Field::editable).unwrap_or(0);
        Form {
            title: title.into(),
            fields,
            focus,
            confirm: None,
            error: None,
            note: None,
            submit_label: "save",
            enter_submits: false,
        }
    }

    /// Names what submitting does in the hints ("save" by default).
    pub fn submit_label(mut self, label: &'static str) -> Form {
        self.submit_label = label;
        self
    }

    /// Enter submits from any field (search-like forms whose later fields are optional
    /// or prefilled); Tab still moves between fields.
    pub fn enter_submits(mut self) -> Form {
        self.enter_submits = true;
        self
    }

    /// Starts on the field `label` instead of the first editable one.
    pub fn focus_on(mut self, label: &str) -> Form {
        if let Some(i) = self.fields.iter().position(|f| f.label == label && f.editable()) {
            self.focus = i;
        }
        self
    }

    /// Requires typing `env_name` before submitting.
    pub fn confirm_with(mut self, env_name: &str) -> Form {
        self.confirm = Some((env_name.to_string(), TextInput::default()));
        self
    }

    pub fn get(&self, label: &str) -> &str {
        self.fields.iter().find(|f| f.label == label).map_or("", Field::value)
    }

    pub fn focused_label(&self) -> Option<&'static str> {
        self.fields.get(self.focus).map(|f| f.label)
    }

    pub fn field_mut(&mut self, label: &str) -> Option<&mut Field> {
        self.fields.iter_mut().find(|f| f.label == label)
    }

    /// Shows a validation or Google Cloud error and keeps the form open.
    pub fn fail(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }

    /// Positions: the fields, then the confirmation input.
    fn stops(&self) -> usize {
        self.fields.len() + usize::from(self.confirm.is_some())
    }

    fn step(&mut self, forward: bool) {
        let n = self.stops();
        for _ in 0..n {
            self.focus = if forward { (self.focus + 1) % n } else { (self.focus + n - 1) % n };
            if self.fields.get(self.focus).is_none_or(Field::editable) {
                break;
            }
        }
    }

    fn validate(&mut self) -> bool {
        if let Some(f) = self.fields.iter().find(|f| f.required && f.editable() && f.value().trim().is_empty()) {
            self.error = Some(format!("{} is required", f.label));
            return false;
        }
        if let Some((name, input)) = &self.confirm
            && input.value.trim() != name
        {
            self.error = Some(format!("Type '{name}' to confirm"));
            self.focus = self.fields.len();
            return false;
        }
        true
    }

    pub fn handle(&mut self, key: KeyEvent) -> FormEvent {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return FormEvent::Cancel,
            KeyCode::Tab | KeyCode::Down => self.step(true),
            KeyCode::BackTab | KeyCode::Up => self.step(false),
            KeyCode::Char('r') if ctrl => {
                if let Some(f) = self.fields.get_mut(self.focus).filter(|f| matches!(f.kind, Kind::Secret)) {
                    f.masked = !f.masked;
                }
            }
            KeyCode::Enter => {
                if self.focus + 1 < self.stops() && !self.enter_submits {
                    self.step(true);
                } else if self.validate() {
                    return FormEvent::Submit;
                }
            }
            _ => {
                if self.focus == self.fields.len() {
                    if let Some((_, input)) = &mut self.confirm {
                        input.handle(key);
                    }
                } else if let Some(f) = self.fields.get_mut(self.focus) {
                    match f.kind {
                        Kind::Choice(options) => match key.code {
                            KeyCode::Left => f.choice = (f.choice + options.len() - 1) % options.len(),
                            KeyCode::Right | KeyCode::Char(' ') => f.choice = (f.choice + 1) % options.len(),
                            _ => {}
                        },
                        Kind::Fixed => {}
                        _ => {
                            if f.input.handle(key) == InputEvent::Changed {
                                self.error = None;
                            }
                        }
                    }
                }
            }
        }
        FormEvent::Pending
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(f) = self.fields.get_mut(self.focus) {
            match f.kind {
                Kind::Text => f.input.insert_str(text, false),
                Kind::Secret => f.input.insert_str(text, true),
                _ => {}
            }
        } else if let Some((_, input)) = &mut self.confirm {
            input.insert_str(text, false);
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect, color: Color) {
        let label_w = self.fields.iter().map(|f| f.label.chars().count()).max().unwrap_or(4) + 2;
        let mut lines = vec![Line::raw("")];
        let mut cursor = None;
        for (i, field) in self.fields.iter().enumerate() {
            let focused = i == self.focus;
            let label = Span::styled(
                format!("{:<label_w$}", field.label),
                if focused { theme::accent().bold() } else { theme::muted() },
            );
            let mut spans = vec![label];
            match field.kind {
                Kind::Choice(options) => {
                    for (j, option) in options.iter().enumerate() {
                        let style = if j == field.choice {
                            Style::new().fg(Color::Black).bg(if focused { theme::ACCENT } else { theme::MUTED })
                        } else {
                            theme::muted()
                        };
                        spans.push(Span::styled(format!(" {option} "), style));
                        spans.push(Span::raw(" "));
                    }
                }
                Kind::Fixed => spans.push(Span::styled(field.input.value.clone(), theme::bold())),
                _ => {
                    let (line, col) = field.input.line_with("› ", field.placeholder, field.masked);
                    if focused {
                        cursor = Some((lines.len(), label_w + col as usize));
                    }
                    spans.extend(line.spans);
                }
            }
            lines.push(Line::from(spans));
        }
        if let Some((name, input)) = &self.confirm {
            lines.push(Line::raw(""));
            let focused = self.focus == self.fields.len();
            lines.push(Line::styled(format!("Type '{name}' to confirm"), Style::new().fg(theme::WARN)));
            let (line, col) = input.line("› ", "");
            if focused {
                cursor = Some((lines.len(), col as usize));
            }
            lines.push(line);
        }
        if let Some(note) = &self.note {
            lines.push(Line::raw(""));
            lines.push(Line::styled(note.clone(), theme::muted()));
        }
        if let Some(e) = &self.error {
            lines.push(Line::raw(""));
            lines.push(Line::styled(format!("✗ {e}"), Style::new().fg(theme::ERR)));
        }
        lines.push(Line::raw(""));
        let mut hints = vec![("Tab", "next field"), ("Enter", self.submit_label), ("Esc", "cancel")];
        if self.fields.iter().any(|f| matches!(f.kind, Kind::Choice(_))) {
            hints.insert(1, ("←→", "choose"));
        }
        if self.fields.iter().any(|f| matches!(f.kind, Kind::Secret)) {
            hints.insert(1, ("^R", "show"));
        }
        lines.push(widgets::hints_line(&hints));

        let width = 84;
        let rect = widgets::popup_area(area, width, widgets::wrapped_height(&lines, width - 4) + 2);
        widgets::popup(f, rect, &self.title, color, Text::from(lines));
        if let Some((row, col)) = cursor {
            // Rows before the cursor do not wrap: field lines are short except the value,
            // which is the line the cursor is on.
            let x = (rect.x + 2 + col as u16).min(rect.right().saturating_sub(2));
            f.set_cursor_position((x, rect.y + 1 + row as u16));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(form: &mut Form, code: KeyCode) -> FormEvent {
        form.handle(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn typing(form: &mut Form, text: &str) {
        for c in text.chars() {
            press(form, KeyCode::Char(c));
        }
    }

    #[test]
    fn fills_validates_and_confirms() {
        let mut form = Form::new(
            "New",
            vec![
                Field::text("Name", "/app/"),
                Field::choice("Type", &["String", "SecureString"], "String"),
                Field::secret("Value", ""),
                Field::text("Description", "").optional("optional"),
            ],
        )
        .confirm_with("prod");
        typing(&mut form, "db");
        press(&mut form, KeyCode::Tab);
        press(&mut form, KeyCode::Right);
        press(&mut form, KeyCode::Tab);
        form.paste("line1\nline2");
        press(&mut form, KeyCode::Tab);
        press(&mut form, KeyCode::Tab);
        typing(&mut form, "pro");
        assert!(matches!(press(&mut form, KeyCode::Enter), FormEvent::Pending));
        typing(&mut form, "d");
        assert!(matches!(press(&mut form, KeyCode::Enter), FormEvent::Submit));
        assert_eq!(form.get("Name"), "/app/db");
        assert_eq!(form.get("Type"), "SecureString");
        assert_eq!(form.get("Value"), "line1\nline2");
        assert_eq!(form.get("Description"), "");
    }

    #[test]
    fn enter_can_submit_from_any_field() {
        let mut form =
            Form::new("Find", vec![Field::text("Key", "c-1"), Field::text("Sort", "").optional("")]).enter_submits();
        assert!(matches!(press(&mut form, KeyCode::Enter), FormEvent::Submit));
        let mut form = form.confirm_with("prod");
        assert!(matches!(press(&mut form, KeyCode::Enter), FormEvent::Pending));
        assert_eq!(form.error.as_deref(), Some("Type 'prod' to confirm"));
    }

    #[test]
    fn required_fields_block_submit() {
        let mut form = Form::new("New", vec![Field::text("Name", ""), Field::secret("Value", "")]);
        press(&mut form, KeyCode::Tab);
        assert!(matches!(press(&mut form, KeyCode::Enter), FormEvent::Pending));
        assert_eq!(form.error.as_deref(), Some("Name is required"));
    }
}
