# What the shell transport runs at a location (docs/PLAN.md §6.2). The whole of this file goes
# to `sh -s` on its stdin, followed by the requests of one call: lines that call the functions
# below, between a `{` and a `} </dev/null`, so the shell has read everything before it starts
# anything and no tool can take the script for its input. Nothing is installed and nothing is
# written at the location.
#
# It has to run in any POSIX sh (dash, bash, zsh, busybox) with GNU, BSD or busybox tools:
# - every expansion is quoted, so nothing depends on how a shell splits words;
# - every path a request names is quoted by the caller and absolute or starts with `./`, so
#   none is taken for an option;
# - what GNU, BSD and busybox tools do differently is settled in `flavour_of_stat`, `lacks`
#   and `clock`, and said in a comment wherever else one of them needs a way around.
#
# The reply, on stdout, is a sequence of records the caller can read without trusting a name
# or a file's bytes. A record is a line of ASCII fields, followed, where the line's first
# letter says so, by text that ends at a NUL byte (a name, a tool's complaint: neither can
# hold one) or by a block of bytes whose length the caller asked for. No name or file content
# is ever found by looking for a newline or a space.

# Bytes, not characters, and the tools' messages in one language.
LC_ALL=C
export LC_ALL
# The reply stays reachable as fd 3 where stdout is a pipe to another tool.
exec 3>&1
# Not inherited from the location's environment: `flavour_of_stat` sets them.
stat_flag='' stat_format='' size_format=''

# lacks <tool>...: "T <tool>" for each that is not installed. `sha256` stands for whichever
# tool computes SHA-256 here, and makes `hash_start <length> <file>` hash a file's start with
# it. (A function for each tool, since a command kept in a variable would need splitting.)
lacks() {
    sb_missing=''
    for sb_tool do
        if [ "$sb_tool" = sha256 ]; then
            if command -v sha256sum >/dev/null 2>&1; then
                hash_start() { head -c "$1" -- "$2" | sha256sum; }
            elif command -v shasum >/dev/null 2>&1; then
                hash_start() { head -c "$1" -- "$2" | shasum -a 256; }
            else
                printf 'T sha256sum or shasum\n'
                sb_missing=1
            fi
        elif ! command -v "$sb_tool" >/dev/null 2>&1; then
            printf 'T %s\n' "$sb_tool"
            sb_missing=1
        fi
    done
}

# need <tool>...: the first thing every call does. With a tool missing it names it and ends
# the reply; otherwise "K", and the records follow.
need() {
    lacks "$@"
    if [ -n "$sb_missing" ]; then
        printf 'Z\n'
        exit 0
    fi
    printf 'K\n'
}

# Whether $1 is seconds and nine digits of nanoseconds.
with_nanoseconds() {
    case $1 in
    '' | *[!0-9.]*) return 1 ;;
    *.?????????) return 0 ;;
    *) return 1 ;;
    esac
}

# Finds out how this location's `stat` is asked for a file's mode in hex, its size and its
# modification time, which it then prints as one line of three numbers per file:
# `stat "$stat_flag" "$stat_format" -- <file>...`. The time has nanoseconds where the tool
# can give them, and is whole seconds otherwise. `$size_format` asks for the size alone.
flavour_of_stat() {
    [ -z "$stat_flag" ] || return 0
    if with_nanoseconds "$(stat -c %.9Y / 2>/dev/null)"; then
        # GNU coreutils.
        stat_flag=-c stat_format='%f %s %.9Y' size_format=%s
    elif stat -c %Y / >/dev/null 2>&1; then
        # busybox, which has no nanoseconds to give.
        stat_flag=-c stat_format='%f %s %Y' size_format=%s
    elif with_nanoseconds "$(stat -f %Fm / 2>/dev/null)"; then
        # BSD and macOS.
        stat_flag=-f stat_format='%Xp %z %Fm' size_format=%z
    else
        # A BSD whose stat has no %F.
        stat_flag=-f stat_format='%Xp %z %m' size_format=%z
    fi
    # `list` runs stat from a shell of find's.
    export stat_flag stat_format
}

# home: "H", then the user's home folder.
home() {
    sb_home=${HOME-}
    case $sb_home in
    /*) ;;
    *)
        # A launcher that sets up no environment: ~name reads the password database.
        sb_user=$(id -un 2>/dev/null)
        case $sb_user in
        '' | *[!A-Za-z0-9._-]*) ;;
        *) eval "sb_home=~$sb_user" ;;
        esac
        ;;
    esac
    printf 'H\n%s\0' "$sb_home"
}

# var <NAME>: "V <NAME>", then its value, when the environment has the variable. The caller
# sends only names that are identifiers.
var() {
    eval "sb_set=\${$1+yes} sb_value=\${$1-}"
    [ -z "$sb_set" ] || printf 'V %s\n%s\0' "$1" "$sb_value"
}

# clock: "C <seconds since the epoch>" by this location's clock, as seconds.nanoseconds where
# its date can tell nanoseconds (GNU, busybox, a recent macOS) and whole seconds otherwise.
clock() {
    sb_now=$(date +%s%N 2>/dev/null)
    case $sb_now in
    # A date that has no %N prints the N itself, or nothing for it.
    *[!0-9]*) sb_now='' ;;
    # Ten digits of seconds or more, and nine of nanoseconds.
    ??????????*?????????) ;;
    *) sb_now='' ;;
    esac
    if [ -n "$sb_now" ]; then
        # All but the last nine digits, a dot, and the last nine.
        printf 'C %s.%s\n' "${sb_now%?????????}" "${sb_now#"${sb_now%?????????}"}"
    else
        printf 'C %s\n' "$(date +%s)"
    fi
}

# file <L|P> <path>: "S <code>", then what stat printed. With L a symbolic link is followed,
# with P it is looked at itself. Codes: 0, stat printed the mode, size and time; 1, nothing is
# there; 2, stat's complaint follows.
file() {
    flavour_of_stat
    if [ "$1" = L ]; then
        # BSD stat -L describes a link whose target is missing, where it should fail.
        if sb_out=$(stat -L "$stat_flag" "$stat_format" -- "$2" 2>&1) && [ -e "$2" ]; then
            sb_code=0
        elif [ -e "$2" ]; then
            sb_code=2
        else
            sb_code=1
        fi
    elif sb_out=$(stat "$stat_flag" "$stat_format" -- "$2" 2>&1); then
        sb_code=0
    elif [ -e "$2" ] || [ -L "$2" ]; then
        sb_code=2
    else
        sb_code=1
    fi
    printf 'S %s\n%s\0' "$sb_code" "$sb_out"
}

# list <folder>: everything under the folder, however deep, without following a symbolic link
# below it. First "C <seconds>", the clock before the listing began. Then find hands the
# names over in batches, each a record:
#   "B <count>", the names, each ended by a NUL, stat's line for each in the same order, "."
# With fewer lines than names, a file went between find and stat and the lines can't be
# matched to the names: the caller asks about those names one by one (`file P`).
# For each folder this user may not read or may not search: "U", then its name. A find that
# can't look inside a folder does not always say so (BSD find skips one that can be read but
# not searched without a word), and what it holds must not be taken for gone.
# Last "F <find's exit status>", then what find complained of.
list() {
    flavour_of_stat
    clock
    if [ ! -d "$1" ]; then
        if [ -e "$1" ] || [ -L "$1" ]; then
            printf 'F 1\nit is not a folder\0'
        else
            printf 'F 1\nit is not there\0'
        fi
        return 0
    fi
    # -H: the folder itself may be a symbolic link. find's complaints go to the variable and
    # the records to the reply. Each inner shell is given as many names as fit a command
    # line, and always exits 0, so that find's exit status speaks of find alone.
    sb_out=$(find -H "$1" -exec sh -c '
        printf "B %s\n" "$#"
        printf "%s\0" "$@"
        stat "$stat_flag" "$stat_format" -- "$@" 2>/dev/null
        printf ".\n"
        exit 0' sh {} + -type d -exec sh -c '
        for sb_folder do
            [ -r "$sb_folder" ] && [ -x "$sb_folder" ] || printf "U\n%s\0" "$sb_folder"
        done
        exit 0' sh {} + 2>&1 >&3)
    printf 'F %s\n%s\0' "$?" "$sb_out"
}

# prefix <length> <path>: "P <there> <size>", then what the hashing printed: one line that
# starts with the SHA-256 of the file's first <length> bytes, and with it or instead of it
# what the tools complained of. <there> is 1 while the path exists; <size> is the file's whole
# size once it was hashed, or "-". The caller compares the size with <length>: head hashes
# less from a shorter file. A <length> of 0 hashes nothing (BSD head refuses a count of 0)
# and prints nothing, or that the file can't be read.
prefix() {
    flavour_of_stat
    if [ "$1" -gt 0 ]; then
        sb_sum=$(hash_start "$1" "$2" 2>&1)
    elif [ -r "$2" ] && [ ! -d "$2" ]; then
        sb_sum=''
    else
        sb_sum='it cannot be read'
    fi
    sb_size=$(stat -L "$stat_flag" "$size_format" -- "$2" 2>/dev/null) || sb_size=''
    if [ -e "$2" ]; then sb_there=1; else sb_there=0; fi
    printf 'P %s %s\n%s\0' "$sb_there" "${sb_size:--}" "$sb_sum"
}

# range <offset> <length> <path>: "R", then a block of exactly <length> + 1 bytes: the bytes
# of the range as far as the file has them, one "x", and zeros to fill the block. So the block
# is as long as the caller expects whatever became of the file meanwhile, and the last byte in
# it that is not zero is the "x", which says how many bytes came from the file. Then
# "E <there>" and what the reading tool complained of.
range() {
    printf 'R\n'
    sb_out=$({
        {
            if [ "$2" -eq 0 ]; then
                # Nothing to read (BSD head refuses a count of 0), but it must be readable.
                [ -r "$3" ] && [ ! -d "$3" ] || echo 'it cannot be read' >&4
            elif [ "$1" -eq 0 ]; then
                head -c "$2" -- "$3" 2>&4
            else
                # tail counts bytes from 1.
                tail -c "+$(($1 + 1))" -- "$3" 2>&4 | head -c "$2"
            fi
            printf x
            head -c "$2" /dev/zero 2>/dev/null
        } | head -c "$(($2 + 1))" >&3
    } 4>&1)
    if [ -e "$3" ]; then sb_there=1; else sb_there=0; fi
    printf 'E %s\n%s\0' "$sb_there" "$sb_out"
}

# small <path>: a small file whole. "M 0 <size>" and the file as `range` sends it; or "M 1",
# nothing is there; or "M 2" and stat's complaint.
small() {
    flavour_of_stat
    if sb_size=$(stat -L "$stat_flag" "$size_format" -- "$1" 2>&1); then
        printf 'M 0 %s\n' "$sb_size"
        range 0 "$sb_size" "$1"
    elif [ -e "$1" ]; then
        printf 'M 2\n%s\0' "$sb_size"
    else
        printf 'M 1\n'
    fi
}

# probe: the connection test. "T <tool>" for each tool the transport needs that is missing,
# "K", the home folder, the clock, and "N 1" when modification times come with nanoseconds
# ("N 0": whole seconds).
probe() {
    lacks find stat head tail date sha256
    printf 'K\n'
    home
    if command -v date >/dev/null 2>&1; then clock; else printf 'C 0\n'; fi
    if command -v stat >/dev/null 2>&1; then flavour_of_stat; fi
    case $stat_format in
    *%.9Y | *%Fm) printf 'N 1\n' ;;
    *) printf 'N 0\n' ;;
    esac
}
