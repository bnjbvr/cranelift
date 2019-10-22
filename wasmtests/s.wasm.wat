(module
 (type $0 (func (param i32) (result i32)))
 (memory $0 1)
 (table $0 0 funcref)
 (export "fibonacci" (func $0))
 (func $0 (; 0 ;) (type $0) (param $0 i32) (result i32)
  (local $1 i32)
  (local $2 i32)
  (local $3 i32)
  (local $4 i32)
  (i32.store
   (i32.const 4)
   (i32.const 1)
  )
  (block $label$1
   (br_if $label$1
    (i32.lt_s
     (local.get $0)
     (i32.const 1)
    )
   )
   (i32.store
    (i32.const 0)
    (i32.const 0)
   )
   (loop $label$2
    (local.set $3
     (i32.add
      (i32.load
       (i32.const 0)
      )
      (i32.load
       (i32.const 4)
      )
     )
    )
    (local.set $4
     (i32.load
      (i32.const 4)
     )
    )
    (i32.store
     (i32.const 0)
     (i32.load
      (i32.const 4)
     )
    )
    (i32.store
     (i32.const 4)
     (local.get $3)
    )
    (br_if $label$2
     (local.tee $0
      (i32.add
       (local.get $0)
       (i32.const -1)
      )
     )
    )
   )
   (return
    (local.get $4)
   )
  )
  (i32.const 0)
 )
)

