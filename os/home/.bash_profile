[ -f ~/.bashrc ] && . ~/.bashrc
if [ "$(tty)" = /dev/tty1 ] && [ -z "$DISPLAY" ]; then
  startx -- -nolisten tcp
  echo "The OTW desktop has ended (log: ~/otw.log). Type 'startx' to start it again."
fi
