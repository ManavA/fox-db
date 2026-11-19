; foxdb tags for plain JavaScript only (TypeScript wraps these in its own clauses).
; `class Child extends Base`: a named supertype.
(class_heritage [(identifier) @name (member_expression property: (property_identifier) @name)]) @reference.implementation
; A class field: found by name, never a call target.
(field_definition property: (property_identifier) @name) @definition.field
