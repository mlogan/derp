# Record from the repository root, after cargo build --release --workspace:
#   vhs docs/demo/demo.tape

Output docs/demo/demo.gif
Set Shell zsh
Set FontSize 16
Set Width 1100
Set Height 700
Set Padding 20
Set TypingSpeed 35ms
Set Theme "Catppuccin Mocha"

Hide
Type "setopt interactive_comments nonomatch; cd docs/demo && export PATH=$PWD/../../target/release:$PATH PS1='$ ' && rm -f counter counter.rw* && clear"
Enter
Show

Type "cat counter.c"
Enter

Type "cc -O1 -o counter counter.c"
Enter
Sleep 500ms

Type "# natively, two threads racing on a counter give a different answer each time"
Enter
Type "./counter; ./counter; ./counter"
Enter
Sleep 2s

Type "# under derp, the seed decides the interleaving"
Enter
Type "derp run --seed 1 --mem-hook-rate 1 ./counter"
Enter
Sleep 2s
Type "derp run --seed 1 --mem-hook-rate 1 ./counter"
Enter
Sleep 5s

Hide
Type "rm -f counter counter.rw*"
Enter
