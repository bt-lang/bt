(module
    (memory (export "memory") 1)

    (func (export "bts_alloc") (param $len i32) (result i32)
        i32.const 1024
    )

    (func (export "bts_free") (param i32) (param i32))

    (data (i32.const 16) "\00\03\2a\00\00\00\00\00\00\00")

    (func (export "bts_call") (param $call_id i32) (param i32) (param i32) (result i64)
        (block $done
            local.get $call_id
            i32.const 1
            i32.ne
            br_if $done
            (loop $spin
                br $spin
            )
        )
        i64.const 68719476746
    )
)
