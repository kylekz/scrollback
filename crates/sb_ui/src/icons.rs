//! scrollback's icons (DESIGN §1.5): the Lucide glyphs the views use and the Windows caption
//! glyphs, embedded from `assets/icons/`.
//!
//! gpui paints `svg()` as a mask tinted with the text colour, so every icon is single-colour.
//! Draw one with `components::icon`; [`crate::Assets`] serves the bytes.
//!
//! Where the files come from:
//! - The Lucide icons are [Lucide](https://lucide.dev)'s geometry (ISC, with some derived from
//!   Feather, MIT: `assets/icons/LICENSE-LUCIDE`) on its 24 grid at the design boards' stroke of
//!   1.6, as `design/project/sb.css` draws them.
//! - The `window-*` caption glyphs are dv's: a 10 grid at a 1px stroke.

macro_rules! sb_icons {
    ($($variant:ident => $file:literal,)*) => {
        /// An icon in `assets/icons/`.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum SbIcon {
            $($variant,)*
        }

        impl SbIcon {
            /// Every icon, in declaration order.
            pub const ALL: &[SbIcon] = &[$(SbIcon::$variant,)*];

            /// The asset path, e.g. `icons/search.svg`.
            pub const fn path(self) -> &'static str {
                match self {
                    $(SbIcon::$variant => concat!("icons/", $file, ".svg"),)*
                }
            }
        }

        /// Every icon file as `(asset path, bytes)`, for [`crate::Assets`].
        pub(crate) const FILES: &[(&str, &[u8])] = &[
            $((
                concat!("icons/", $file, ".svg"),
                include_bytes!(concat!("../../../assets/icons/", $file, ".svg")).as_slice(),
            ),)*
        ];
    };
}

sb_icons! {
    // Windows caption buttons.
    WindowMinimize => "window-minimize",
    WindowMaximize => "window-maximize",
    WindowRestore => "window-restore",
    WindowClose => "window-close",
    // Lucide.
    Archive => "archive",
    ArrowLeft => "arrow-left",
    Bot => "bot",
    Box => "box",
    Check => "check",
    ChevronDown => "chevron-down",
    ChevronRight => "chevron-right",
    ChevronUp => "chevron-up",
    CircleAlert => "circle-alert",
    CircleCheck => "circle-check",
    Copy => "copy",
    FilePlus => "file-plus",
    FileSearch => "file-search",
    FileText => "file-text",
    FileX => "file-x",
    FoldVertical => "fold-vertical",
    Folder => "folder",
    FolderPlus => "folder-plus",
    Globe => "globe",
    Hand => "hand",
    Image => "image",
    Inbox => "inbox",
    Info => "info",
    Laptop => "laptop",
    Layers => "layers",
    Lightbulb => "lightbulb",
    Link => "link",
    ListTodo => "list-todo",
    Monitor => "monitor",
    PanelLeft => "panel-left",
    Pause => "pause",
    Pen => "pen",
    Plug => "plug",
    Plus => "plus",
    // A tool call of no kind in particular (DESIGN §1.5 "puzzle", as the boards draw it).
    Puzzle => "puzzle",
    RefreshCw => "refresh-cw",
    Search => "search",
    Settings => "settings",
    Star => "star",
    // The star of a starred session: filled (DESIGN §4.2.1).
    StarFilled => "star-filled",
    Terminal => "terminal",
    Trash => "trash-2",
    TriangleAlert => "triangle-alert",
    X => "x",
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_an_embedded_svg() {
        assert_eq!(SbIcon::ALL.len(), FILES.len());
        for (icon, (path, bytes)) in SbIcon::ALL.iter().zip(FILES) {
            assert_eq!(icon.path(), *path);
            let text = std::str::from_utf8(bytes).expect("an SVG is UTF-8 text");
            assert!(
                text.trim_start().starts_with("<svg"),
                "{path} is not an SVG"
            );
            assert!(
                !text.contains("fill=\"#") && !text.contains("stroke=\"#"),
                "{path} hard-codes a colour; icons take the text colour (currentColor)"
            );
        }
    }
}
