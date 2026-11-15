;; A sample plugin built from `wit/mission-control.wit`, written in the component model's text
;; format (Ruling R99). Permission: `register-types`.
;;
;; On init it registers the post type `lcbook` and then traps (`unreachable`), as a
;; plugin with a bug in its main file does (Task 12.4).
(component
  (type $types-type (instance
    (export "register-post-type" (func (param "definition-json" string) (result (result (error string)))))
  ))
  (import "mission-control:plugin/types@0.1.0" (instance $types (type $types-type)))

  ;; The memory and the allocator the host copies strings in with.
  (core module $libc
    (memory (export "memory") 1)
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

  (core func $register-post-type
    (canon lower (func $types "register-post-type") (memory $mem) (realloc $realloc)))

  (core module $main
    (import "libc" "memory" (memory 1))
    (import "types" "register-post-type" (func $register-post-type (param i32 i32 i32)))
    ;; 64: the post type (17 bytes).
    (data (i32.const 64) "{\22slug\22:\22lcbook\22}")

    (func (export "init") (result i32)
      (call $register-post-type (i32.const 64) (i32.const 17) (i32.const 256))
      unreachable)

    (func (export "on-action") (param i32 i32 i32 i32) (result i32)
      unreachable)

    (func (export "apply-filter") (param i32 i32 i32 i32 i32 i32) (result i32)
      unreachable)

    ;; Ruling R105: `activate` and `deactivate` do nothing; 8: where they write their result.
    (func (export "activate") (result i32)
      (i32.store8 (i32.const 8) (i32.const 0))
      (i32.const 8))

    (func (export "deactivate") (result i32)
      (i32.store8 (i32.const 8) (i32.const 0))
      (i32.const 8))
  )
  (core instance $main (instantiate $main
    (with "libc" (instance $libc))
    (with "types" (instance (export "register-post-type" (func $register-post-type))))
  ))

  (func $init (result (result (error string)))
    (canon lift (core func $main "init") (memory $mem) (realloc $realloc)))
  (func $activate (result (result (error string)))
    (canon lift (core func $main "activate") (memory $mem) (realloc $realloc)))
  (func $deactivate (result (result (error string)))
    (canon lift (core func $main "deactivate") (memory $mem) (realloc $realloc)))
  (func $on-action (param "name" string) (param "payload-json" string) (result (result (error string)))
    (canon lift (core func $main "on-action") (memory $mem) (realloc $realloc)))
  (func $apply-filter (param "name" string) (param "value-json" string) (param "args-json" string)
    (result (result string (error string)))
    (canon lift (core func $main "apply-filter") (memory $mem) (realloc $realloc)))
  (instance $handler
    (export "init" (func $init))
    (export "activate" (func $activate))
    (export "deactivate" (func $deactivate))
    (export "on-action" (func $on-action))
    (export "apply-filter" (func $apply-filter))
  )
  (export "mission-control:plugin/handler@0.1.0" (instance $handler))
)
