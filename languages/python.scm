(module (expression_statement (assignment left: (identifier) @name) @definition.constant))

(class_definition
  name: (identifier) @name) @definition.class

(function_definition
  name: (identifier) @name) @definition.function

(call
  function: [
      (identifier) @name
      (attribute
        attribute: (identifier) @name)
  ]) @reference.call
(import_statement name: (dotted_name) @name) @reference.import
(import_from_statement name: (dotted_name) @name) @reference.import
(import_from_statement module_name: (dotted_name) @name) @reference.import
; A function or bound method passed as an argument or keyword value (`connect(self.opened)`,
; `callback=parse`), and a base class: a reference. Parameters and locals are dropped.
(argument_list [(identifier) @name (attribute attribute: (identifier) @name)]) @reference.ref
(keyword_argument value: [(identifier) @name (attribute attribute: (identifier) @name)]) @reference.ref
