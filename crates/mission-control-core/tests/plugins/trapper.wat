;; A sample plugin built from `wit/mission-control.wit`, written in the component model's text
;; format (Ruling R99). Permission: `hooks`.
;;
;; On init it subscribes to the action `mc_test_trap`; its handlers then trap
;; (`unreachable`), as a plugin with a bug does.
(component
  (type $hooks-type (instance
    (export "add-action" (func (param "name" string) (param "priority" s32) (result (result (error string)))))
    (export "add-filter" (func (param "name" string) (param "priority" s32) (result (result (error string)))))
  ))
  (import "mission-control:plugin/hooks@0.1.0" (instance $hooks (type $hooks-type)))

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

  (core func $add-action (canon lower (func $hooks "add-action") (memory $mem) (realloc $realloc)))

  (core module $main
    (import "libc" "memory" (memory 1))
    (import "hooks" "add-action" (func $add-action (param i32 i32 i32 i32)))
    ;; 16: the action's name (12 bytes).
    (data (i32.const 16) "mc_test_trap")
    ;; 256: where imports write their results.

    (func (export "init") (result i32)
      (call $add-action (i32.const 16) (i32.const 12) (i32.const 10) (i32.const 256))
      ;; add-action's `result<_, string>` is laid out as init's: pass it on.
      (i32.const 256))

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
    (with "hooks" (instance (export "add-action" (func $add-action))))
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
