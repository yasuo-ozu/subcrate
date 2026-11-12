//! The three macros expand normally (outside `cargo doc`).

use doc_macros::{hidden_func, my_attr, my_func, MyDerive};

#[my_attr]
#[derive(MyDerive)]
struct Item {
    #[helper]
    _field: u32,
}

#[test]
fn the_macros_expand() {
    assert_eq!(my_func!(), 1);
    assert_eq!(hidden_func!(), 2);
    assert_eq!(derived(), 7);
    let _ = Item { _field: 0 };
}
