;; A sample plugin built from `wit/mission-control.wit`, written in the component model's text
;; format (Ruling R99). No permission.
;;
;; Its memory starts at 1025 pages of 64 KiB, just over 64 MiB, the default limit. It does
;; nothing else.
(component
  ;; The memory and the allocator the host copies strings in with; it traps when the memory
  ;; cannot grow, as Rust's allocator aborts.
  (core module $libc
    (memory (export "memory") 1025)
    (global $heap (mut i32) (i32.const 4096))
    (func (export "cabi_realloc") (param i32 i32) (param $align i32) (param $size i32) (result i32)
      (local $ptr i32)
      (local.set $ptr
        (i32.and
          (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
          (i32.sub (i32.const 0) (local.get $align))))
      (global.set $heap (i32.add (local.get $ptr) (local.get $size)))
      (if (i32.gt_u (global.get $heap) (i32.mul (memory.size) (i32.const 65536)))
        (then
          (if (i32.eq
                (memory.grow
                  (i32.shr_u
                    (i32.add
                      (i32.sub (global.get $heap) (i32.mul (memory.size) (i32.const 65536)))
                      (i32.const 65535))
                    (i32.const 16)))
                (i32.const -1))
            (then unreachable))))
      (local.get $ptr))
  )
  (core instance $libc (instantiate $libc))
  (alias core export $libc "memory" (core memory $mem))
  (alias core export $libc "cabi_realloc" (core func $realloc))

  (core module $main
    (import "libc" "memory" (memory 1))

    (func (export "activate") (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    (func (export "on-action") (param i32 i32 i32 i32) (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    (func (export "apply-filter") (param i32 i32 i32 i32 i32 i32) (result i32)
      unreachable)
  )
  (core instance $main (instantiate $main
    (with "libc" (instance $libc))
  ))

  (func $activate (result (result (error string)))
    (canon lift (core func $main "activate") (memory $mem) (realloc $realloc)))
  (func $on-action (param "name" string) (param "payload-json" string) (result (result (error string)))
    (canon lift (core func $main "on-action") (memory $mem) (realloc $realloc)))
  (func $apply-filter (param "name" string) (param "value-json" string) (param "args-json" string)
    (result (result string (error string)))
    (canon lift (core func $main "apply-filter") (memory $mem) (realloc $realloc)))
  (instance $handler
    (export "activate" (func $activate))
    (export "on-action" (func $on-action))
    (export "apply-filter" (func $apply-filter))
  )
  (export "mission-control:plugin/handler@0.1.0" (instance $handler))
)
