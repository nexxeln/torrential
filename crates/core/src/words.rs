//! Fieldless enums whose wire names are written once.

/// Defines a fieldless enum and the word each value is on the wire. Serde, `as_str` and `ALL`
/// all read that one table, so the three cannot drift apart.
#[macro_export]
macro_rules! words {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident = $word:literal, )*
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Debug, ::serde::Serialize, ::serde::Deserialize)]
        $vis enum $name {
            $( $(#[$variant_meta])* #[serde(rename = $word)] $variant, )*
        }

        // A binary that never lists every value, or never prints one, leaves these unused.
        #[allow(dead_code)]
        impl $name {
            /// Every value, in the order they are written.
            pub const ALL: [Self; [$(stringify!($variant)),*].len()] = [$(Self::$variant),*];

            /// The word for this value on the wire.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $word,)*
                }
            }
        }
    };
}
