// ABOUTME: Language-aware prefix extraction for completion systems
// ABOUTME: Combines Helix's LSP integration with Zed's sophisticated character classification

use std::collections::HashSet;

/// Language-aware completion character classifier inspired by Zed's approach
/// but optimized for LSP integration like Helix
pub struct PrefixExtractor {
    /// Characters that can be part of identifiers (alphanumeric + language-specific)
    identifier_chars: HashSet<char>,
    /// Characters that trigger method/property completion (dots, arrows, etc)
    trigger_chars: HashSet<char>,
    /// Characters that separate completion contexts (whitespace, operators)
    separator_chars: HashSet<char>,
}

impl Default for PrefixExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl PrefixExtractor {
    pub fn new() -> Self {
        // Base identifier characters (Helix approach)
        let mut identifier_chars = HashSet::new();
        for c in 'a'..='z' {
            identifier_chars.insert(c);
        }
        for c in 'A'..='Z' {
            identifier_chars.insert(c);
        }
        for c in '0'..='9' {
            identifier_chars.insert(c);
        }
        identifier_chars.insert('_');

        // Language-specific identifier extensions (Zed approach)
        identifier_chars.insert('-'); // CSS properties, Lisp
        identifier_chars.insert('$'); // PHP, JavaScript
        identifier_chars.insert('@'); // Annotations, decorators

        // Completion trigger characters
        let mut trigger_chars = HashSet::new();
        trigger_chars.insert('.'); // Method/property access
        trigger_chars.insert(':'); // CSS, namespace resolution
        trigger_chars.insert('>'); // Arrow operator (part of ->)

        // Separator characters (end completion context)
        let mut separator_chars = HashSet::new();
        separator_chars.insert(' ');
        separator_chars.insert('\t');
        separator_chars.insert('\n');
        separator_chars.insert('(');
        separator_chars.insert(')');
        separator_chars.insert('[');
        separator_chars.insert(']');
        separator_chars.insert('{');
        separator_chars.insert('}');
        separator_chars.insert(',');
        separator_chars.insert(';');

        Self {
            identifier_chars,
            trigger_chars,
            separator_chars,
        }
    }

    pub fn line_prefix_at_char(line_text: &str, cursor_col: usize) -> &str {
        if cursor_col == 0 || line_text.is_empty() {
            return "";
        }

        line_text
            .char_indices()
            .nth(cursor_col)
            .map(|(byte_index, _)| &line_text[..byte_index])
            .unwrap_or(line_text)
    }

    /// Extract completion prefix from text at cursor position
    /// Returns (prefix, is_trigger_completion)
    pub fn extract_prefix(&self, line_text: &str, cursor_col: usize) -> (String, bool) {
        if cursor_col == 0 || line_text.is_empty() {
            return (String::new(), false);
        }

        let line_text_to_cursor = Self::line_prefix_at_char(line_text, cursor_col);
        let chars: Vec<char> = line_text_to_cursor.chars().collect();

        if chars.is_empty() {
            return (String::new(), false);
        }

        // Check if we're in a trigger completion context (e.g., "obj.method")
        let is_trigger_completion = self.is_trigger_context(&chars);

        if is_trigger_completion {
            // For trigger completions, prefix starts after the trigger character
            self.extract_trigger_prefix(&chars)
        } else {
            // For normal completions, extract the current identifier
            self.extract_identifier_prefix(&chars)
        }
    }

    fn is_trigger_context(&self, chars: &[char]) -> bool {
        // Look for trigger characters walking backwards until we hit a separator
        for i in (0..chars.len()).rev() {
            let c = chars[i];
            if self.trigger_chars.contains(&c) {
                return true;
            }
            if self.separator_chars.contains(&c) {
                break; // Hit separator before trigger
            }
        }
        false
    }

    fn extract_trigger_prefix(&self, chars: &[char]) -> (String, bool) {
        // Find the most recent trigger character
        for i in (0..chars.len()).rev() {
            if self.trigger_chars.contains(&chars[i]) {
                // Extract everything after the trigger as prefix
                let prefix: String = chars[i + 1..].iter().collect();
                return (prefix, true);
            }
        }

        // Fallback to identifier extraction if no trigger found
        self.extract_identifier_prefix(chars)
    }

    fn extract_identifier_prefix(&self, chars: &[char]) -> (String, bool) {
        // Walk backwards to find the start of the current identifier
        let mut start_pos = chars.len();

        for i in (0..chars.len()).rev() {
            let c = chars[i];
            if self.identifier_chars.contains(&c) {
                start_pos = i;
            } else {
                break;
            }
        }

        let prefix: String = chars[start_pos..].iter().collect();
        (prefix, false)
    }

    /// Language-specific configuration for different file types
    pub fn configure_for_language(&mut self, language: &str) {
        match language {
            "rust" => {
                // Don't add ':' to identifier_chars - it should trigger completion
                self.trigger_chars.insert(':');
            }
            "javascript" | "typescript" => {
                self.identifier_chars.insert('$');
                self.trigger_chars.insert('.');
            }
            "css" | "scss" | "less" => {
                self.identifier_chars.insert('-');
                self.trigger_chars.insert(':');
            }
            "php" => {
                self.identifier_chars.insert('$');
                self.trigger_chars.insert('-'); // For ->
            }
            "c" | "cpp" => {
                self.trigger_chars.insert('-'); // For ->
                self.trigger_chars.insert(':'); // For ::
            }
            _ => {} // Use defaults
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_identifier_extraction() {
        let extractor = PrefixExtractor::new();

        // Basic word completion
        let (prefix, is_trigger) = extractor.extract_prefix("let variable_name", 17);
        assert_eq!(prefix, "variable_name");
        assert!(!is_trigger);

        // Partial word
        let (prefix, is_trigger) = extractor.extract_prefix("let var", 7);
        assert_eq!(prefix, "var");
        assert!(!is_trigger);
    }

    #[test]
    fn test_prefix_extraction_uses_character_cursor_offsets() {
        let extractor = PrefixExtractor::new();

        assert_eq!(PrefixExtractor::line_prefix_at_char("é.value", 1), "é");

        let (prefix, is_trigger) = extractor.extract_prefix("é.value", 3);
        assert_eq!(prefix, "v");
        assert!(is_trigger);
    }

    #[test]
    fn test_dot_notation_completion() {
        let extractor = PrefixExtractor::new();

        // Method completion after dot
        let (prefix, is_trigger) = extractor.extract_prefix("client.method", 13);
        assert_eq!(prefix, "method");
        assert!(is_trigger);

        // Empty prefix right after dot
        let (prefix, is_trigger) = extractor.extract_prefix("client.", 7);
        assert_eq!(prefix, "");
        assert!(is_trigger);

        // Chained method calls
        let (prefix, is_trigger) = extractor.extract_prefix("obj.method().another", 20);
        assert_eq!(prefix, "another");
        assert!(is_trigger);
    }

    #[test]
    fn test_language_specific_features() {
        let mut extractor = PrefixExtractor::new();
        extractor.configure_for_language("rust");

        // Rust namespace resolution
        let (prefix, is_trigger) = extractor.extract_prefix("std::collections::HashMap", 25);
        assert_eq!(prefix, "HashMap");
        assert!(is_trigger);
    }

    #[test]
    fn test_separators_end_completion() {
        let extractor = PrefixExtractor::new();

        // Parentheses should end completion context
        let (prefix, is_trigger) = extractor.extract_prefix("function(param", 14);
        assert_eq!(prefix, "param");
        assert!(!is_trigger);

        // Comma should end completion context
        let (prefix, is_trigger) = extractor.extract_prefix("func(a, b", 9);
        assert_eq!(prefix, "b");
        assert!(!is_trigger);
    }

    // The four tests below pin the `(prefix, is_trigger_completion)` contract that
    // `workspace::should_dismiss_completion_menu` depends on to tell "the cursor moved to a
    // fresh line" (dismiss the completion menu) apart from "the user just typed a trigger
    // character" (keep it). An empty prefix alone cannot distinguish the two, so these
    // expectations are derived from `extract_prefix`'s control flow rather than from whatever
    // it happens to return today.

    /// Fresh line with no auto-indent, cursor at column 0: expected `("", false)` -> dismiss.
    ///
    /// `extract_prefix` early-returns `("", false)` for `cursor_col == 0`
    /// (`prefix_extraction.rs:85-87`) before it inspects the line at all, so `"\n"` and `""`
    /// both yield an empty prefix with no trigger context. That is exactly the cursor state
    /// after Enter with no indentation, and it must dismiss the menu instead of filtering with
    /// `""` (which matches every item and leaves the popup open).
    #[test]
    fn test_fresh_line_at_column_zero_is_empty_and_not_a_trigger() {
        let mut extractor = PrefixExtractor::new();
        extractor.configure_for_language("rust");

        let (prefix, is_trigger) = extractor.extract_prefix("\n", 0);
        assert_eq!(prefix, "");
        assert!(!is_trigger);

        let (prefix, is_trigger) = extractor.extract_prefix("", 0);
        assert_eq!(prefix, "");
        assert!(!is_trigger);
    }

    /// Fresh line with a 4-space auto-indent, cursor at column 4: expected `("", false)` ->
    /// dismiss.
    ///
    /// The text before the cursor is the indent run `"    "`. `is_trigger_context` walks
    /// backwards and hits `' '`, which is a separator rather than a trigger character, so it
    /// stops immediately and reports no trigger. `extract_identifier_prefix` also breaks on its
    /// very first step, because the character directly before the cursor is whitespace and
    /// whitespace is not an identifier character; `start_pos` therefore stays at
    /// `chars.len()` and the extracted prefix is empty rather than the raw whitespace run.
    ///
    /// Either way the dismissal rule holds, because it tests `prefix.trim().is_empty()`, and the
    /// cursor sits mid-line inside the indent run so this case cannot be caught by looking for
    /// a newline or by comparing line numbers.
    #[test]
    fn test_fresh_line_with_auto_indent_is_empty_and_not_a_trigger() {
        let mut extractor = PrefixExtractor::new();
        extractor.configure_for_language("rust");

        let (prefix, is_trigger) = extractor.extract_prefix("    \n", 4);
        assert_eq!(prefix, "");
        assert!(!is_trigger);

        // A tab-indented fresh line behaves identically.
        let (prefix, is_trigger) = extractor.extract_prefix("\t", 1);
        assert_eq!(prefix, "");
        assert!(!is_trigger);
    }

    /// Immediately after `obj.`: expected `("", true)` -> keep the menu.
    ///
    /// `"javascript"` is configured here because it is the language whose
    /// `configure_for_language` explicitly inserts `'.'` into `trigger_chars`; `.` is also a
    /// base trigger character, so the flag does not depend on that call. `is_trigger_context`
    /// walks back from the cursor and finds `'.'` before any separator, so
    /// `extract_trigger_prefix` takes over and returns everything after the trigger character,
    /// which is nothing. This empty prefix is the documented, intended member-completion
    /// behaviour and must not be treated as a fresh line.
    #[test]
    fn test_trigger_character_yields_empty_but_flagged_prefix() {
        let mut extractor = PrefixExtractor::new();
        extractor.configure_for_language("javascript");

        let (prefix, is_trigger) = extractor.extract_prefix("obj.", 4);
        assert_eq!(prefix, "");
        assert!(is_trigger);
    }

    /// Mid-word while typing `st`: expected `("st", false)` -> keep the menu.
    ///
    /// `is_trigger_context` walks back over the identifier characters and then stops at the
    /// `' '` separator in `"let st"`, so there is no trigger. `extract_identifier_prefix`
    /// walks back over `'s'` and `'t'` to the separator at index 3 and returns
    /// `chars[3..]`, i.e. the word being typed. A non-empty prefix keeps the menu open and
    /// refines the filter as usual.
    #[test]
    fn test_word_being_typed_is_kept_as_a_non_trigger_prefix() {
        let mut extractor = PrefixExtractor::new();
        extractor.configure_for_language("rust");

        let (prefix, is_trigger) = extractor.extract_prefix("let st", 7);
        assert_eq!(prefix, "st");
        assert!(!is_trigger);
    }
}
