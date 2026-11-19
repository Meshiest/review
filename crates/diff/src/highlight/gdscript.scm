(comment) @comment
(string) @string
(string_name) @string
(escape_sequence) @string.escape
(integer) @number
(float) @number
[(true) (false) (null)] @constant.builtin

(identifier) @variable

(function_definition name: (name) @function)
(call (identifier) @function)
(attribute_call (identifier) @function)
(base_call (identifier) @function)
(signal_statement name: (name) @function)

(class_definition name: (name) @type)
(class_name_statement name: (name) @type)
(type (identifier) @type)
(extends_statement (type (identifier) @type))

(const_statement name: (name) @constant)
(enumerator left: (identifier) @constant)
(enum_definition (name) @type)

(typed_parameter (identifier) @variable.parameter)
(default_parameter (identifier) @variable.parameter)

(annotation "@" @attribute (identifier) @attribute)
(get_node) @string
(node_path) @string

[(static_keyword) (remote_keyword)] @keyword

[
  "and" "as" "await" "break" "class" "class_name" "const" "continue" "elif"
  "else" "enum" "export" "extends" "for" "func" "get" "if" "in" "is" "match"
  "not" "onready" "or" "pass" "return" "set" "setget" "signal" "var" "when"
  "while"
] @keyword

[
  "!" "!=" "%" "%=" "&" "&&" "&=" "*" "**" "**=" "*=" "+" "+=" "-" "-=" "->"
  "/" "/=" ":=" "<" "<<" "<<=" "<=" "=" "==" ">" ">=" ">>" ">>=" "^" "^=" "|"
  "|=" "||" "~"
] @operator

["(" ")" "[" "]" "{" "}" "," ":" ";" "."] @punctuation
