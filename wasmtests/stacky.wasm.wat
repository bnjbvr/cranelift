(module
 (type $0 (func (param i32 i32) (result i32)))
 (memory $0 256 256)
 (export "add" (func $0))
 (func $0 (; 0 ;) (type $0) (param $0 i32) (param $1 i32) (result i32)
  (local $2 i32)
  (i32.add
   (block (result i32)
    (local.set $2
     (local.get $0)
    )
    (local.set $0
     (i32.const 100)
    )
    (local.get $2)
   )
   (local.get $1)
  )
 )
)

