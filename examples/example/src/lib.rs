use subcrate::subcrate;

#[subcrate(proc_macro = true)]
pub mod my_macro {
    use proc_macro::TokenStream;

    #[proc_macro]
    pub fn mac(_: TokenStream) -> TokenStream {
        "42".parse().unwrap()
    }
}

// A single ordinary library crate can export a proc-macro *and* normal items.
// Lifting the macro to the crate root means downstream users can write
// `example::mac!()` right next to `example::calculate()` — something a plain
// `proc-macro` crate (which may export nothing but macros) cannot do.
pub use my_macro::mac;

#[subcrate]
pub mod my_crate {
    pub fn calc() -> usize {
        32
    }
}

// A subcrate can depend on another: `my_crate` is linked into `doubled` (under
// that name), so `doubled` is compiled after it and can call into it.
#[subcrate(my_crate)]
pub mod doubled {
    pub fn calc() -> usize {
        my_crate::calc() * 2
    }
}

pub mod nested {
    use subcrate::subcrate;

    #[subcrate]
    pub mod inner {
        pub fn answer() -> usize {
            42
        }
    }
}

pub fn calculate() -> usize {
    my_crate::calc() + nested::inner::answer()
}
