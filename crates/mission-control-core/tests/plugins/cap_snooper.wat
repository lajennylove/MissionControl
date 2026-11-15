;; A sample plugin built from `wit/mission-control.wit`, written in the component model's text
;; format (Ruling R99). Permission: `hooks`.
;;
;; On activation it tries to subscribe to the filter `user_has_cap`, which the host refuses to
;; every plugin (Ruling R102); activate passes the refusal on.
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

  (core func $add-filter (canon lower (func $hooks "add-filter") (memory $mem) (realloc $realloc)))

  (core module $main
    (import "libc" "memory" (memory 1))
    (import "hooks" "add-filter" (func $add-filter (param i32 i32 i32 i32)))
    ;; 16: the filter's name (12 bytes); 32: the new value (10 bytes).
    (data (i32.const 16) "user_has_cap")
    (data (i32.const 32) "\22Filtered\22")
    ;; 256: where imports write their results; 512: where exports write theirs.

    (func (export "activate") (result i32)
      (call $add-filter (i32.const 16) (i32.const 12) (i32.const 10) (i32.const 256))
      ;; add-filter's `result<_, string>` is laid out as activate's: pass it on.
      (i32.const 256))

    (func (export "on-action") (param i32 i32 i32 i32) (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    (func (export "apply-filter") (param i32 i32 i32 i32 i32 i32) (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.store (i32.const 516) (i32.const 32))
      (i32.store (i32.const 520) (i32.const 10))
      (i32.const 512))
  )
  (core instance $main (instantiate $main
    (with "libc" (instance $libc))
    (with "hooks" (instance (export "add-filter" (func $add-filter))))
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
