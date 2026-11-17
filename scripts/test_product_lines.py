"""Tests of scripts/product-lines.py (NFR-MAINT-12): the counting rule
for the product's source lines. Every expected count is worked out by
hand in a comment beside the fixture.

Run with `python3 -m unittest discover -s scripts -p 'test_*.py'`.
"""

import contextlib
import importlib.util
import io
import os
import tempfile
import textwrap
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_SPEC = importlib.util.spec_from_file_location("product_lines", os.path.join(_HERE, "product-lines.py"))
pl = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(pl)


def src(text):
    return textwrap.dedent(text).lstrip("\n")


class Comments(unittest.TestCase):
    def test_blank_lines_and_line_comments_are_not_counted(self):
        text = src(
            """
            //! The crate's doc comment.

            /// An item's doc comment.
            // A plain comment.
            pub fn f() -> u8 {   // counted: code before the comment
                1
            }
            """
        )
        # Counted: the three lines of `f`.
        self.assertEqual(pl.count_text(text), 3)

    def test_block_comments_are_not_counted(self):
        text = src(
            """
            /* one line */
            /*
             * several lines
             */
            /** a block doc comment */
            /* outer /* nested */ still comment */
            let a = 1; /* code first */
            /* code after */ let b = 2;
            let c /* inside */ = 3;
            """
        )
        # Counted: the lines of `a`, `b` and `c`.
        self.assertEqual(pl.count_text(text), 3)

    def test_a_comment_opener_inside_a_string_is_code(self):
        text = src(
            r'''
            let url = "http://example.org/"; // the comment
            let glob = "/*";
            let quote = "a \" // still a string";
            let raw = r#"a "quoted" // string /* too"#;
            let b = 1;
            '''
        )
        # Counted: all five lines; none opens a comment.
        self.assertEqual(pl.count_text(text), 5)

    def test_a_multi_line_string_is_code(self):
        text = src(
            """
            const HELP: &str = "first
            // not a comment
            last";
            """
        )
        # Counted: the three lines of the string.
        self.assertEqual(pl.count_text(text), 3)


class InlineTestModules(unittest.TestCase):
    def test_an_inline_test_module_is_dropped(self):
        text = src(
            """
            pub fn add(a: u8, b: u8) -> u8 {
                a + b
            }

            #[cfg(test)]
            mod tests {
                use super::*;

                #[test]
                fn adds() {
                    assert_eq!(add(1, 2), 3);
                }
            }

            pub fn after() {}
            """
        )
        # Counted: `add` (3) and `after` (1).
        self.assertEqual(pl.count_text(text), 4)

    def test_braces_in_strings_comments_and_chars_do_not_end_the_module(self):
        text = src(
            r'''
            #[cfg(test)]
            mod tests {
                // a stray } in a comment
                /* and { another */
                const S: &str = "}}}";
                const R: &str = r#"}"#;
                const C: char = '}';
                const E: char = '\'';
                fn lifetime<'a>(x: &'a str) -> &'a str {
                    x
                }
            }
            pub fn kept() {}
            '''
        )
        # Counted: `kept` only.
        self.assertEqual(pl.count_text(text), 1)

    def test_further_attributes_between_cfg_test_and_mod(self):
        text = src(
            """
            #[cfg(test)]
            #[allow(clippy::unwrap_used)]
            // a comment between
            #[expect(dead_code, reason = "a ] in a string")]
            pub(crate) mod tests {
                fn f() {}
            }
            fn kept() {}
            """
        )
        # Counted: `kept` only.
        self.assertEqual(pl.count_text(text), 1)

    def test_nested_test_modules_and_code_after_them(self):
        text = src(
            """
            mod outer {
                #[cfg(test)]
                mod a {
                    #[cfg(test)]
                    mod b {}
                }
                fn kept() {}
            }
            """
        )
        # Counted: `mod outer {`, `fn kept`, the closing brace.
        self.assertEqual(pl.count_text(text), 3)

    def test_cfg_test_inside_a_comment_or_string_is_ignored(self):
        text = src(
            """
            // #[cfg(test)]
            const S: &str = "#[cfg(test)]";
            mod kept {
                fn f() {}
            }
            """
        )
        # Counted: `S` and the three lines of `kept`.
        self.assertEqual(pl.count_text(text), 4)

    def test_other_cfgs_are_product_code(self):
        text = src(
            """
            #[cfg(not(test))]
            fn real() {}
            #[cfg(any(test, fuzzing))]
            fn shared() {}
            #[cfg( test )]
            fn spaced() {}
            """
        )
        # Counted: the first four lines; `cfg( test )` is `cfg(test)`.
        self.assertEqual(pl.count_text(text), 4)


class CfgTestOnOtherItems(unittest.TestCase):
    def test_a_use_and_a_fn(self):
        text = src(
            """
            #[cfg(test)]
            pub(crate) use crate::x::y;
            #[cfg(test)]
            pub(crate) fn len(&self) -> usize {
                self.v.len()
            }
            #[cfg(test)]
            fn generic<A, B>(a: A, b: B) -> [u8; 2] {
                [0; 2]
            }
            fn kept() {}
            """
        )
        # Counted: `kept` only.
        self.assertEqual(pl.count_text(text), 1)

    def test_a_struct_field_a_literal_field_and_a_statement(self):
        text = src(
            """
            pub struct Epoll {
                fd: OwnedFd,
                #[cfg(test)]
                ctl_calls: CtlCalls,
            }
            #[cfg(test)]
            #[derive(Debug, Clone, Default)]
            pub struct CtlCalls(Arc<AtomicU64>);
            fn new() -> Epoll {
                Epoll {
                    fd,
                    #[cfg(test)]
                    ctl_calls: CtlCalls::default(),
                }
            }
            fn ctl(&self) {
                #[cfg(test)]
                self.ctl_calls
                    .0
                    .fetch_add(1, Ordering::Relaxed);
                let x = 1;
            }
            """
        )
        # Counted: `Epoll` (3: open, `fd`, close), `new` (5: fn, open,
        # `fd`, two closes), `ctl` (3: fn, `let`, close).
        self.assertEqual(pl.count_text(text), 11)

    def test_a_last_field_without_a_comma(self):
        text = src(
            """
            pub struct S {
                a: u8,
                #[cfg(test)]
                b: u8
            }
            """
        )
        # Counted: open, `a`, close.
        self.assertEqual(pl.count_text(text), 3)


class KnownApproximations(unittest.TestCase):
    """The documented errors of the sketch, pinned so a change to them is
    a decision, not an accident."""

    def test_an_attribute_before_cfg_test_stays_counted(self):
        text = src(
            """
            #[derive(Debug)]
            #[cfg(test)]
            struct S;
            """
        )
        # Over: the `derive` line.
        self.assertEqual(pl.count_text(text), 1)

    def test_a_comma_in_angle_brackets_ends_a_statement_early(self):
        text = src(
            """
            fn f() {
                #[cfg(test)]
                let m: HashMap<u8,
                    u8> = HashMap::new();
            }
            """
        )
        # Over: the statement's second line (the fn's two lines are right).
        self.assertEqual(pl.count_text(text), 3)

    def test_code_sharing_a_line_with_a_test_item_is_dropped(self):
        text = "fn a() {} #[cfg(test)] mod t {} fn b() {}\nfn c() {}\n"
        # Under: `a` and `b` share the dropped line.
        self.assertEqual(pl.count_text(text), 1)

    def test_an_inner_cfg_test_is_not_recognised(self):
        text = "#![cfg(test)]\nfn a() {}\n"
        # Over: both lines.
        self.assertEqual(pl.count_text(text), 2)


def write(root, rel, text):
    path = os.path.join(root, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(src(text))


class Tree(unittest.TestCase):
    def test_test_files_and_directories_are_skipped(self):
        with tempfile.TemporaryDirectory() as d:
            root = os.path.join(d, "src")
            # 2 lines.
            write(root, "lib.rs", "pub mod a;\npub mod core;\n")
            # 3 lines: the inline module is dropped.
            write(root, "a.rs", "fn a() {\n}\n#[cfg(test)]\nmod tests;\n#[cfg(test)]\nmod a_tests;\nfn b() {}\n")
            # Declared `#[cfg(test)] mod tests;` and `mod a_tests;` by a.rs.
            write(root, "a/tests.rs", "fn t() {}\n")
            write(root, "a/a_tests.rs", "fn t() {}\n")
            write(root, "a/a_tests/deeper.rs", "fn t() {}\n")
            # core/mod.rs declares its test modules beside itself: 2 lines.
            write(root, "core/mod.rs", "#[cfg(test)]\npub(crate) mod test_support;\n#[cfg(test)]\nmod flow_tests;\nmod kept;\npub fn k() {}\n")
            write(root, "core/test_support.rs", "pub fn helper() {}\n")
            write(root, "core/flow_tests.rs", "fn t() {}\n")
            # 1 line.
            write(root, "core/kept.rs", "fn kept() {}\n")
            # By name alone, without a declaration.
            write(root, "x/tests.rs", "fn t() {}\n")
            write(root, "x/tests/mod.rs", "fn t() {}\n")
            write(root, "x/test_support/mod.rs", "fn t() {}\n")
            write(root, "x/test_support.rs", "fn t() {}\n")
            # Not Rust.
            write(root, "notes.txt", "text\n")
            self.assertEqual(pl.count_tree([root]), 2 + 3 + 2 + 1)

    def test_a_product_file_whose_name_only_contains_tests_is_counted(self):
        with tempfile.TemporaryDirectory() as d:
            write(d, "src/latests.rs", "fn a() {}\n")
            write(d, "src/tests_helper/x.rs", "fn b() {}\n")
            self.assertEqual(pl.count_tree([os.path.join(d, "src")]), 2)

    def test_several_roots_add_up(self):
        with tempfile.TemporaryDirectory() as d:
            write(d, "one/src/lib.rs", "fn a() {}\nfn b() {}\n")
            write(d, "two/src/main.rs", "fn main() {}\n")
            roots = [os.path.join(d, "one/src"), os.path.join(d, "two/src")]
            self.assertEqual(pl.count_tree(roots), 3)


class CommandLine(unittest.TestCase):
    def run_main(self, args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = pl.main(args)
        return code, out.getvalue()

    def test_prints_the_count(self):
        with tempfile.TemporaryDirectory() as d:
            write(d, "src/lib.rs", "fn a() {}\n\nfn b() {}\n")
            code, out = self.run_main([os.path.join(d, "src")])
        self.assertEqual((code, out), (0, "2\n"))

    def test_csv_output(self):
        with tempfile.TemporaryDirectory() as d:
            write(d, "src/lib.rs", "fn a() {}\n")
            code, out = self.run_main(
                ["--csv", "--commit", "abc1234", "--date", "2026-11-17", os.path.join(d, "src")])
        self.assertEqual((code, out), (0, "date,commit,lines\n2026-11-17,abc1234,1\n"))

    def test_csv_needs_a_hex_commit_and_a_date(self):
        with tempfile.TemporaryDirectory() as d:
            write(d, "src/lib.rs", "fn a() {}\n")
            root = os.path.join(d, "src")
            cases = (
                ["--csv", root],
                ["--csv", "--commit", "abc1234", root],
                ["--csv", "--commit", "ABC1234", "--date", "2026-11-17", root],
                ["--csv", "--commit", "abc1234", "--date", "17.11.2026", root],
                ["--csv", "--commit", "abc1234", "--date", "2026-13-01", root],
            )
            for args in cases:
                with self.assertRaises(SystemExit, msg=args):
                    with contextlib.redirect_stderr(io.StringIO()), contextlib.redirect_stdout(io.StringIO()):
                        pl.main(args)

    def test_a_missing_root_is_an_error(self):
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(SystemExit):
                with contextlib.redirect_stderr(io.StringIO()):
                    pl.main([os.path.join(d, "nowhere")])

    def test_the_default_roots_are_the_two_product_crates(self):
        self.assertEqual(
            [os.path.relpath(r, pl.REPO) for r in pl.DEFAULT_ROOTS],
            [os.path.join("core", "src"), os.path.join("driver", "src")])


if __name__ == "__main__":
    unittest.main()
