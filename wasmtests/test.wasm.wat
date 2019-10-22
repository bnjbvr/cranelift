(module
 (type $0 (func (param i32) (result f32)))
 (type $1 (func (result i32)))
 (type $2 (func (param i32 i64)))
 (import "foo" "bar" (func $$import (result i32)))
 (table $0 1 1 funcref)
 (elem (i32.const 0) $$import)
 (export "baz" (func $$import))
 (export "quux" (func $$func0))
 (func $$func0 (; 1 ;) (type $0) (param $0 i32) (result f32)
  (f32.const 1)
 )
 (func $ (; 2 ;) (type $2) (param $0 i32) (param $1 i64)
  (local $2 f32)
  (local $3 f64)
  (drop
   (call $$import)
  )
  (drop
   (call $$func0
    (i32.const 0)
   )
  )
  (drop
   (call_indirect (type $0)
    (i32.const 0)
    (i32.const 1)
   )
  )
  (drop
   (local.get $1)
  )
  (local.set $2
   (f32.const 0)
  )
 )
)

