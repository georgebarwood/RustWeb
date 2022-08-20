Axum-based webserver based on rustdb database, with database browsing, 
timed jobs, password hashing, data compression, email transmission and database replication.

USAGE:\
    rustweb.exe [OPTIONS] <PORT>

ARGS:\
    <PORT>    Port to listen on

OPTIONS:\
    -h, --help             Print help information\
    -i, --ip <IP>          Ip Address to listen on [default: 0.0.0.0]\
    -l, --login <LOGIN>    Login cookies for replication [default: ]\
    -m, --mem <MEM>        Memory limit for page cache (in MB) [default: 10]\
    -r, --rep <REP>        Server to replicate [default: ]\
        --tracemem         Trace memory trimming\
        --tracetime        Trace query time\
    -V, --version          Print version information

crates.io : https://crates.io/crates/rustweb

Installation and starting server
================================

cargo install rustweb

cargo run rustweb 3000

This should start the rustweb, listening on port 3000.

You should then be able to browse to http://localhost:3000/Menu

Security
========

Initially security is disabled. To enable it 

(1) Create a record in login.user.

(2) Use the Logins link to set up a password.

(3) Edit handler.get ( see instructions there ).

Database replication
====================

Start Rustweb in the directory (folder) specifying -rer and -login options.

For example:

rustweb --rep https://mydomain.com

If login security has been enabled, you will need to specify login details ( from the login.user table ), for example:

--login "uid=1; hpw="0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

Email
=====

Email can be sent using the email schema.

(1) Ceate a record in email.SmtpServer

(2) Create an email in email.msg

(3) Insert it into email.Queue

(4) Call the builtin function EMAILTX()

If an email cannot be sent, and the error is temporary, it will be inserted into the email.Delayed table and retried later.

Permanent errors are logged in email.SendError

Timed Jobs
==========

A named SQL function (with no paramaters) can be called at a specified time by creating a record in timed.Job.

This is used by the email system to retry temporary email send errors.
 