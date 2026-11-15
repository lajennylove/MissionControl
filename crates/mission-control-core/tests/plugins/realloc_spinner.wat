;; A sample plugin built from `wit/mission-control.wit`, written in the component model's text
;; format (Ruling R99). Permission: `hooks`.
;;
;; On init it subscribes to the filter `mc_test_slow`. Its allocator loops forever, so
;; the filter's call runs out of fuel while the host copies the arguments in, before the
;; plugin's own code runs.
(component
  (type $hooks-type (instance
    (export "add-action" (func (param "name" string) (param "priority" s32) (result (result (error string)))))
    (export "add-filter" (func (param "name" string) (param "priority" s32) (result (result (error string)))))
  ))
  (import "mission-control:plugin/hooks@0.1.0" (instance $hooks (type $hooks-type)))

  ;; The memory, and an allocator that loops forever.
  (core module $libc
    (memory (export "memory") 1)
    (global $heap (mut i32) (i32.const 4096))
    (func (export "cabi_realloc") (param i32 i32) (param $align i32) (param $size i32) (result i32)
      (local $n i32)
      (loop $forever
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $forever))
      unreachable)
  )
  (core instance $libc (instantiate $libc))
  (alias core export $libc "memory" (core memory $mem))
  (alias core export $libc "cabi_realloc" (core func $realloc))

  (core func $add-filter (canon lower (func $hooks "add-filter") (memory $mem) (realloc $realloc)))

  (core module $main
    (import "libc" "memory" (memory 1))
    (import "hooks" "add-filter" (func $add-filter (param i32 i32 i32 i32)))
    ;; 16: the filter's name (12 bytes); 256: where imports write their results.
    (data (i32.const 16) "mc_test_slow")

    (func (export "init") (result i32)
      (call $add-filter (i32.const 16) (i32.const 12) (i32.const 10) (i32.const 256))
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
    (with "hooks" (instance (export "add-filter" (func $add-filter))))
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
