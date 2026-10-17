package(default_visibility = ["//visibility:public"])

genrule(
    name = "hello",
    outs = ["hello_out.txt"],
    cmd = "echo hello > $@",
)

genrule(
    name = "lib_a",
    srcs = ["input.txt"],
    outs = ["a_out.txt"],
    cmd = "cat $(SRCS) > $@",
)

genrule(
    name = "bin_b",
    srcs = ["input.txt", ":lib_a"],
    outs = ["b_out.txt"],
    cmd = "cat $(SRCS) > $@",
)
