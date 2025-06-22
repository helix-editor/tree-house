   /// **hello-world** 
// ┡┛╿╿┡┛┡━━━━━━━━━┛┡┛╰─ comment
// │ │││ │          ╰─ comment markup.bold punctuation.bracket
// │ │││ ╰─ comment markup.bold
// │ ││╰─ comment markup.bold punctuation.bracket
// │ │╰─ comment
// │ ╰─ comment comment
// ╰─ comment
   ///
// ┡┛╿╰─ comment
// │ ╰─ comment comment
// ╰─ comment
   /// ```
// ┡┛╿┡━━┛╰─ comment markup.raw.block
// │ │╰─ comment markup.raw.block punctuation.bracket
// │ ╰─ comment comment
// ╰─ comment
   /// fn foo()
// ┡┛╿╿┡┛╿┡━┛┡┛╰─ comment markup.raw.block
// │ │││ ││  ╰─ comment markup.raw.block punctuation.bracket
// │ │││ │╰─ comment markup.raw.block function
// │ │││ ╰─ comment markup.raw.block
// │ ││╰─ comment markup.raw.block keyword.function
// │ │╰─ comment markup.raw.block
// │ ╰─ comment comment
// ╰─ comment
   /// ```
// ┡┛╿┡━━┛╰─ comment markup.raw.block
// │ │╰─ comment markup.raw.block punctuation.bracket
// │ ╰─ comment comment
// ╰─ comment
   /// **foo
// ┡┛╿╿┡┛┗━┹─ comment markup.bold
// │ ││╰─ comment markup.bold punctuation.bracket
// │ │╰─ comment
// │ ╰─ comment comment
// ╰─ comment
   fn foo() {
// ┡┛ ┡━┛┡┛ ╰─ punctuation.bracket
// │  │  ╰─ punctuation.bracket
// │  ╰─ function
// ╰─ keyword.function
       println!("hello world")
//     ┡━━━━━━┛╿┡━━━━━━━━━━━┛╰─ punctuation.bracket
//     │       │╰─ string
//     │       ╰─ punctuation.bracket
//     ╰─ function.macro
   }
// ╰─ punctuation.bracket
   /// bar**
// ┡┛╿┡━━┛┡┛╰─ comment
// │ ││   ╰─ comment markup.bold punctuation.bracket
// │ │╰─ comment markup.bold
// │ ╰─ comment comment
// ╰─ comment
   fn bar() {
// ┡┛ ┡━┛┡┛ ╰─ punctuation.bracket
// │  │  ╰─ punctuation.bracket
// │  ╰─ function
// ╰─ keyword.function
       println!("hello world")
//     ┡━━━━━━┛╿┡━━━━━━━━━━━┛╰─ punctuation.bracket
//     │       │╰─ string
//     │       ╰─ punctuation.bracket
//     ╰─ function.macro
   }
// ╰─ punctuation.bracket
