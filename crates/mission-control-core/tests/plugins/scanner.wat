;; A sample plugin built from `wit/mission-control.wit`, written in the component model's text
;; format (Ruling R99). Permission: `hooks`.
;;
;; On activation it subscribes to the filter `mc_test_scan`; the filter then reads every byte
;; of the value and gives it back with ASCII lowercase letters made uppercase: the work of a
;; plain filter over a post's content, to measure what a normal call costs in fuel.
(component
  (type $hooks-type (instance
    (export "add-action" (func (param "name" string) (param "priority" s32) (result (result (error string)))))
    (export "add-filter" (func (param "name" string) (param "priority" s32) (result (result (error string)))))
  ))
  (import "mission-control:plugin/hooks@0.1.0" (instance $hooks (type $hooks-type)))

  ;; The memory and the allocator the host copies strings in with; it traps when the memory
  ;; cannot grow, as Rust's allocator aborts.
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
    ;; 16: the filter's name (12 bytes).
    (data (i32.const 16) "mc_test_scan")
    ;; 256: where imports write their results; 512: where exports write theirs.

    (func (export "activate") (result i32)
      (call $add-filter (i32.const 16) (i32.const 12) (i32.const 10) (i32.const 256))
      (i32.const 256))

    (func (export "on-action") (param i32 i32 i32 i32) (result i32)
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.const 512))

    (func (export "apply-filter")
      (param $name i32) (param $name-len i32) (param $value i32) (param $len i32)
      (param $args i32) (param $args-len i32) (result i32)
      (local $at i32) (local $end i32) (local $byte i32)
      (local.set $at (local.get $value))
      (local.set $end (i32.add (local.get $value) (local.get $len)))
      (block $done
        (loop $next
          (br_if $done (i32.ge_u (local.get $at) (local.get $end)))
          (local.set $byte (i32.load8_u (local.get $at)))
          (if (i32.lt_u (i32.sub (local.get $byte) (i32.const 97)) (i32.const 26))
            (then (i32.store8 (local.get $at) (i32.sub (local.get $byte) (i32.const 32)))))
          (local.set $at (i32.add (local.get $at) (i32.const 1)))
          (br $next)))
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.store (i32.const 516) (local.get $value))
      (i32.store (i32.const 520) (local.get $len))
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
