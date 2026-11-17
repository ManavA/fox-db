; foxdb tags for Objective-C.
(class_interface . (identifier) @name) @definition.class
(class_implementation . (identifier) @name) @definition.class
(protocol_declaration . (identifier) @name) @definition.interface
(method_declaration (identifier) @name) @definition.method
(method_definition (identifier) @name) @definition.method
(function_definition declarator: (function_declarator declarator: (identifier) @name)) @definition.function
(message_expression method: (identifier) @name) @reference.call
(call_expression function: (identifier) @name) @reference.call
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))) @definition.function
