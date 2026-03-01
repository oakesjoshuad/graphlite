; Elements with id attributes (DOM anchors and navigation targets)
(
  (element
    (start_tag
      (attribute
        (attribute_name) @_attr
        (quoted_attribute_value
          (attribute_value) @name)))) @definition.module
  (#eq? @_attr "id")
)

; Custom/web component elements (hyphenated tag names = web components)
(
  (element
    (start_tag
      (tag_name) @name)) @definition.class
  (#match? @name ".*-.*")
)