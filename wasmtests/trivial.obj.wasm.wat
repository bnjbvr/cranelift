(module
 (type $0 (func (result i32)))
 (import "env" "bar" (global $gimport$0 i32))
 (import "env" "baz" (global $gimport$1 i32))
 (memory $0 0)
 (table $0 1 funcref)
 (elem (i32.const 0) $foo)
 (export "foo" (func $foo))
 (func $foo (; 0 ;) (type $0) (result i32)
  (local $0 i32)
  (local $1 i32)
  (local $2 i32)
  (local $3 i32)
  (local $4 i32)
  (local $5 i32)
  (local $6 i32)
  (local.set $0
   (i32.const 0)
  )
  (local.set $1
   (i32.const 0)
  )
  (local.set $2
   (i32.load offset=4294967295
    (local.get $1)
   )
  )
  (local.set $3
   (i32.const 0)
  )
  (local.set $4
   (i32.load offset=4294967295
    (local.get $3)
   )
  )
  (local.set $5
   (i32.add
    (local.get $2)
    (local.get $4)
   )
  )
  (local.set $6
   (i32.add
    (local.get $5)
    (local.get $0)
   )
  )
  (return
   (local.get $6)
  )
 )
 ;; custom section "reloc.CODE", size 13
)

