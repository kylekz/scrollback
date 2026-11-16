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
    Ban => "ban",
    Bot => "bot",
    Box => "box",
    Calendar => "calendar",
    Check => "check",
    ChevronDown => "chevron-down",
    ChevronRight => "chevron-right",
    ChevronUp => "chevron-up",
    CircleAlert => "circle-alert",
    CircleCheck => "circle-check",
    Copy => "copy",
    Ellipsis => "ellipsis",
    ExternalLink => "external-link",
    Eye => "eye",
    FilePlus => "file-plus",
    FileSearch => "file-search",
    FileText => "file-text",
    FileX => "file-x",
    Filter => "filter",
    FoldVertical => "fold-vertical",
    Folder => "folder",
    // Move to folder.
    FolderInput => "folder-input",
    FolderPlus => "folder-plus",
    GitBranch => "git-branch",
    Globe => "globe",
    GripVertical => "grip-vertical",
    Hand => "hand",
    HardDrive => "hard-drive",
    History => "history",
    Image => "image",
    Inbox => "inbox",
    Info => "info",
    Keyboard => "keyboard",
    Laptop => "laptop",
    Layers => "layers",
    LayoutGrid => "layout-grid",
    Lightbulb => "lightbulb",
    Link => "link",
    List => "list",
    ListTodo => "list-todo",
    Lock => "lock",
    // A search result (DESIGN §4.9).
    MessageSquare => "message-square",
    Monitor => "monitor",
    PanelLeft => "panel-left",
    PanelRight => "panel-right",
    Pause => "pause",
    Pen => "pen",
    Plug => "plug",
    Plus => "plus",
    // A tool call of no kind in particular (DESIGN §1.5 "puzzle", as the boards draw it).
    Puzzle => "puzzle",
    RefreshCw => "refresh-cw",
    // Restore from Trash.
    RotateCcw => "rotate-ccw",
    Search => "search",
    Settings => "settings",
    ShieldCheck => "shield-check",
    Star => "star",
    // The star of a starred session: filled (DESIGN §4.2.1).
    StarFilled => "star-filled",
    Sun => "sun",
    Terminal => "terminal",
    Trash => "trash-2",
    TriangleAlert => "triangle-alert",
    // What the owner typed: a prompt among search results (`E-Search`).
    User => "user",
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
