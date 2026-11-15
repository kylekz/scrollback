# What the shell transport runs at a location (docs/PLAN.md §6.2). The whole of this file goes
# to `sh -s` on its stdin, followed by the requests of one call: lines that call the functions
# below, between a `{` and a `} </dev/null`, so the shell has read everything before it starts
# anything and no tool can take the script for its input. Nothing is installed and nothing is
# written at the location.
#
# It has to run in any POSIX sh (dash, bash, zsh, busybox) with GNU, BSD or busybox tools:
# - every expansion is quoted, so nothing depends on how a shell splits words; a tool's lines
#   are taken one at a time with `${lines%%"$sb_nl"*}`;
# - every path a request names is quoted by the caller and absolute or starts with `./`, so
#   none is taken for an option;
# - what GNU, BSD and busybox tools do differently is settled in `flavour_of_stat`, `lacks`
#   and `clock`, and said in a comment wherever else one of them needs a way around.
#
# The reply, on stdout, is a sequence of records the caller can read without trusting a name
# or a file's bytes. A record is a line of ASCII fields, followed, where the line's first
# letter says so, by text that ends at a NUL byte (a name, a tool's complaint: neither can
# hold one) or by a block of bytes whose length the line gives or the caller knows. No name
# or file content is ever found by looking for a newline or a space.
#
# Whether a file is missing is never decided here: `test -e` is false for a path that can't
# be looked up for any reason. A tool's complaint goes to the caller, which reads in it
# whether the file is not there or something else is wrong.

# Bytes, not characters, and the tools' messages in one language.
LC_ALL=C
export LC_ALL
# The reply stays reachable as fd 3 where stdout is a pipe to another tool.
exec 3>&1
# Not inherited from the location's environment: `flavour_of_stat` sets them.
stat_flag='' stat_format='' size_format=''
# A newline, to take lines apart at.
sb_nl='
'

# lacks <tool>...: "T <tool>" for each that is not installed. A name that starts with a slash
# is a device that must be readable. `sha256` stands for whichever tool computes SHA-256
# here, and makes `hash_start <length> <file>` hash a file's start with it and
# `hash_files <file>...` hash whole files, one line each. (A function for each tool, since a
# command kept in a variable would need splitting.)
lacks() {
    sb_missing=''
    for sb_tool do
        case $sb_tool in
        sha256)
            if command -v sha256sum >/dev/null 2>&1; then
                hash_start() { head -c "$1" -- "$2" | sha256sum; }
                hash_files() { sha256sum -- "$@"; }
            elif command -v shasum >/dev/null 2>&1; then
                hash_start() { head -c "$1" -- "$2" | shasum -a 256; }
                hash_files() { shasum -a 256 -- "$@"; }
            else
                printf 'T sha256sum or shasum\n'
                sb_missing=1
            fi
            ;;
        /*)
            if [ ! -r "$sb_tool" ]; then
                printf 'T %s\n' "$sb_tool"
                sb_missing=1
            fi
            ;;
        *)
            if ! command -v "$sb_tool" >/dev/null 2>&1; then
                printf 'T %s\n' "$sb_tool"
                sb_missing=1
            fi
            ;;
        esac
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

# Takes the first line off $sb_lines and puts it in $sb_line.
next_line() {
    sb_line=${sb_lines%%"$sb_nl"*}
    case $sb_lines in
    *"$sb_nl"*) sb_lines=${sb_lines#*"$sb_nl"} ;;
    *) sb_lines='' ;;
    esac
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

# as_login <script> <done> <seconds>: runs <script> with `sh -c` from inside the user's login
# shell, so that it sees what their profile exports: a command run by `wsl.exe --exec` or
# `ssh host command` gets none of that. Whatever the profile prints comes out too; the script
# marks its own output. A login shell that takes longer than <seconds> is killed. Last, on a
# line of its own, "<done>".
as_login() {
    # The script travels in the environment, so the login shell, whichever it is, has only
    # a plain command to run.
    SB_LOGIN=$1 "${SHELL:-sh}" -lc 'exec sh -c "$SB_LOGIN"' </dev/null 2>/dev/null &
    sb_login=$!
    # Kills it once the time is up. It holds none of the reply's pipes, and when it is told
    # to stop, takes its sleep with it.
    (
        trap 'kill "$sb_sleep" 2>/dev/null; exit 0' TERM
        sleep "$3" &
        sb_sleep=$!
        wait "$sb_sleep" && kill "$sb_login" 2>/dev/null
    ) </dev/null >/dev/null 2>&1 &
    sb_watch=$!
    wait "$sb_login" 2>/dev/null
    kill "$sb_watch" 2>/dev/null
    wait "$sb_watch" 2>/dev/null
    printf '\n%s\n' "$2"
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
# with P it is looked at itself. Codes: 0, stat printed the mode, size and time; 1, to L, a
# link whose target is missing; 2, stat failed, and what follows is its complaint.
file() {
    flavour_of_stat
    sb_code=2
    if [ "$1" = P ]; then
        sb_out=$(stat "$stat_flag" "$stat_format" -- "$2" 2>&1) && sb_code=0
    elif sb_out=$(stat -L "$stat_flag" "$stat_format" -- "$2" 2>&1); then
        # BSD stat -L describes the link itself where its target is missing.
        if [ -e "$2" ]; then sb_code=0; else sb_code=1; fi
    fi
    printf 'S %s\n%s\0' "$sb_code" "$sb_out"
}

# list <folder>: everything under the folder, however deep, without following a symbolic link
# below it. First "C <seconds>", the clock before the listing began. Then find hands the
# names over in batches, each a record:
#   "B <count>", the names, each ended by a NUL, stat's line for each in the same order, "."
# With fewer lines than names, stat failed on one of them (it went between find and stat, or
# it can't be looked at) and the lines can't be matched to the names: the caller asks about
# those names one by one (`file P`).
# For each folder this user may not read or may not search: "U", then its name. A find that
# can't look inside a folder does not always say so (BSD find skips one that can be read but
# not searched without a word), and what it holds must not be taken for gone.
# Last "F <find's exit status>", then what find complained of.
list() {
    flavour_of_stat
    clock
    if [ ! -d "$1" ]; then
        # What is in the way of listing it, in stat's words; a file has nothing in the way.
        sb_out=$(stat -L "$stat_flag" "$stat_format" -- "$1" 2>&1) && sb_out='it is not a folder'
        printf 'F 1\n%s\0' "$sb_out"
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

# prefix <length> <path>: "P <size>", then what the hashing printed: one line that starts
# with the SHA-256 of the file's first <length> bytes, and with it or instead of it what the
# tools complained of. <size> is the file's whole size once it was hashed, or "-" with stat's
# complaint as the text's last line. The caller compares the size with <length>: head hashes
# less from a shorter file. A <length> of 0 hashes nothing (BSD head refuses a count of 0)
# and prints nothing, unless the file may not be read.
prefix() {
    flavour_of_stat
    if [ "$1" -gt 0 ]; then
        sb_sum=$(hash_start "$1" "$2" 2>&1)
    elif [ -e "$2" ] && [ ! -r "$2" ]; then
        sb_sum='Permission denied'
    else
        sb_sum=''
    fi
    if sb_size=$(stat -L "$stat_flag" "$size_format" -- "$2" 2>&1); then
        printf 'P %s\n%s\0' "$sb_size" "$sb_sum"
    else
        printf 'P -\n%s\n%s\0' "$sb_sum" "$sb_size"
    fi
}

# hashed <file>...: "W <count>", then what the hashing tool printed for the files, a line
# each with the hash and the file's name, and after a NUL each file's size once all were
# hashed. Nothing when there are no files.
hashed() {
    [ "$#" -gt 0 ] || return 0
    sb_sums=$(hash_files "$@" 2>/dev/null)
    sb_after=$(stat -L "$stat_flag" "$size_format" -- "$@" 2>/dev/null)
    printf 'W %s\n%s\0%s\0' "$#" "$sb_sums" "$sb_after"
}

# wholes <lengths> <path>...: what `prefix` gives for each path with its length (the lengths
# are one word, a space after each), with one process for many files where it can be had: a
# file that is exactly as long as its prefix is hashed whole, with its neighbours that are
# (`hashed`), and any other by itself (`prefix`), all in the order given. The caller holds
# each line of a "W" record to its file's name and length, and asks again by `prefix` for a
# run that does not hold.
wholes() {
    flavour_of_stat
    sb_lengths=$1
    shift
    sb_lines=$(stat -L "$stat_flag" "$size_format" -- "$@" 2>/dev/null)
    # With a file that stat failed on, its lines can't be matched to the files: none is
    # taken for whole.
    sb_count=0 sb_sizes=$sb_lines
    while [ -n "$sb_lines" ]; do
        next_line
        sb_count=$((sb_count + 1))
    done
    [ "$sb_count" -eq "$#" ] || sb_sizes=''
    # The loop goes through the paths as they were given; the positional parameters are
    # emptied at its first turn and hold the run of whole files gathered so far.
    sb_first=1
    for sb_path do
        if [ -n "$sb_first" ]; then
            set --
            sb_first=''
        fi
        sb_length=${sb_lengths%% *} sb_lengths=${sb_lengths#* }
        sb_lines=$sb_sizes
        next_line
        sb_sizes=$sb_lines
        if [ "$sb_line" = "$sb_length" ]; then
            set -- "$@" "$sb_path"
        else
            hashed "$@"
            set --
            prefix "$sb_length" "$sb_path"
        fi
    done
    hashed "$@"
}

# range <offset> <length> <path>: "R <count>", then a block of exactly <count> + 1 bytes: the
# bytes of the range as far as the file has them, one "x", and zeros to fill the block. <count>
# is the range's length, or as much of it as the file was long enough for when it was looked
# at. So the block is as long as it says whatever became of the file meanwhile, and the last
# byte in it that is not zero is the "x", which says how many bytes came from the file. Then
# "E" and what stat or the reading tool complained of.
range() {
    flavour_of_stat
    sb_out='' sb_block=0
    if ! sb_size=$(stat -L "$stat_flag" "$size_format" -- "$3" 2>&1); then
        sb_out=$sb_size
    elif [ ! -r "$3" ]; then
        sb_out='Permission denied'
    elif [ "$sb_size" -le "$1" ]; then
        :
    elif [ "$((sb_size - $1))" -lt "$2" ]; then
        sb_block=$((sb_size - $1))
    else
        sb_block=$2
    fi
    printf 'R %s\n' "$sb_block"
    if [ "$sb_block" -gt 0 ]; then
        # The reader's complaint goes to the variable (fd 4), the block to the reply.
        sb_out=$({
            {
                if [ "$1" -eq 0 ]; then
                    head -c "$sb_block" -- "$3" 2>&4
                else
                    # dd seeks to the megabyte the range starts in, where tail would read
                    # its way there (and busybox's fails past 2 GiB); tail drops the start
                    # of that megabyte, counting bytes from 1.
                    dd if="$3" bs=1048576 skip="$(($1 / 1048576))" 2>/dev/null |
                        tail -c "+$(($1 % 1048576 + 1))" | head -c "$sb_block"
                fi
                printf x
                head -c "$sb_block" /dev/zero 2>/dev/null
            } | head -c "$((sb_block + 1))" >&3
        } 4>&1)
    else
        printf x
    fi
    printf 'E\n%s\0' "$sb_out"
}

# bulk <limit> <lengths> <path>...: the whole of each file, for many small files at once. The
# lengths are one word, a space after each: what `range 0 <length> <path>` would be asked.
# With every file a regular one that stat can look at, and no more than <limit> bytes in them
# all: "G <count> <total>", stat's line for each file, then a block of <total> + 1 bytes: the
# files one after another, one "x", and zeros to fill the block; then stat's lines once more,
# ended by ".". The caller cuts the block by the sizes in stat's lines, and asks again file by
# file if the "x" is not where they add up to or a file is not as stat had it before.
# Otherwise "G 0 0", and each file as `range` gives it.
bulk() {
    flavour_of_stat
    sb_limit=$1 sb_lengths=$2
    shift 2
    sb_stats=$(stat -L "$stat_flag" "$stat_format" -- "$@" 2>/dev/null)
    sb_total=0 sb_count=0 sb_lines=$sb_stats
    while [ -n "$sb_lines" ]; do
        next_line
        # A regular file's mode, in hex, is 8 and three digits more.
        case $sb_line in
        8*) ;;
        *)
            sb_count=-1
            break
            ;;
        esac
        sb_size=${sb_line#* }
        sb_total=$((sb_total + ${sb_size%% *})) sb_count=$((sb_count + 1))
    done
    if [ "$sb_count" -eq "$#" ] && [ "$sb_total" -le "$sb_limit" ]; then
        printf 'G %s %s\n%s\n' "$#" "$sb_total" "$sb_stats"
        {
            cat -- "$@" 2>/dev/null
            printf x
            head -c "$sb_total" /dev/zero 2>/dev/null
        } | head -c "$((sb_total + 1))"
        stat -L "$stat_flag" "$stat_format" -- "$@" 2>/dev/null
        printf '.\n'
    else
        printf 'G 0 0\n'
        for sb_path do
            range 0 "${sb_lengths%% *}" "$sb_path"
            sb_lengths=${sb_lengths#* }
        done
    fi
}

# small <path>: a small file whole. "M 0 <size>" and the file as `range` sends it; or "M 2"
# and stat's complaint.
small() {
    flavour_of_stat
    if sb_size=$(stat -L "$stat_flag" "$size_format" -- "$1" 2>&1); then
        printf 'M 0 %s\n' "$sb_size"
        range 0 "$sb_size" "$1"
    else
        printf 'M 2\n%s\0' "$sb_size"
    fi
}

# probe: the connection test. "T <tool>" for each tool the transport needs that is missing,
# "K", the home folder, the clock, "N 1" when modification times come with nanoseconds ("N 0":
# whole seconds), "L", then where the shell that runs this is found and, after a NUL, the
# user's login shell, and last /etc/wsl.conf as `small` gives a file: a WSL distro says there
# where it mounts the Windows drives. Without the tools to read it, "M 2" and nothing.
probe() {
    lacks find stat head tail dd cat date sha256 /dev/zero
    printf 'K\n'
    home
    if command -v date >/dev/null 2>&1; then clock; else printf 'C 0\n'; fi
    if command -v stat >/dev/null 2>&1; then flavour_of_stat; fi
    case $stat_format in
    *%.9Y | *%Fm) printf 'N 1\n' ;;
    *) printf 'N 0\n' ;;
    esac
    printf 'L\n%s\0%s\0' "$(command -v sh)" "${SHELL:-sh}"
    if [ -z "$sb_missing" ]; then small /etc/wsl.conf; else printf 'M 2\n\0'; fi
}
