#include <assert.h>
#include <stdarg.h>
#include <string.h>

#define ENABLE_LIBRARY
#include "../.deps/hev-socks5-tunnel/src/hev-main.c"
#include "../.deps/hev-socks5-tunnel/src/hev-socks5-session.c"

static const int configured_connect_timeout = 5000;
static const int configured_tcp_data_timeout = -1;
static const int configured_udp_data_timeout = 120000;
static int installed_connect_timeout;
static int installed_handshake_timeout;
static int installed_udp_timeout;
static int expected_splice_timeout;
static int expected_connect_port;
static int connect_calls;
static int splice_calls;
static int fail_connect;

static HevConfigServer server = {
    .port = 31416,
    .udp_port = 31418,
    .addr = "127.0.0.1",
};

static void
splice (HevSocks5Session *session)
{
    assert (HEV_SOCKS5 (session)->timeout == expected_splice_timeout);
    splice_calls++;
}

static HevSocks5SessionIface session_iface = {
    .splicer = splice,
};

static void *
object_iface (HevObject *object, void *type)
{
    (void)object;
    (void)type;
    return &session_iface;
}

static HevObjectClass object_class = {
    .iface = object_iface,
};

int
hev_config_get_misc_connect_timeout (void)
{
    return configured_connect_timeout;
}

int
hev_config_get_misc_tcp_read_write_timeout (void)
{
    return configured_tcp_data_timeout;
}

int
hev_config_get_misc_udp_read_write_timeout (void)
{
    return configured_udp_data_timeout;
}

void
hev_socks5_set_connect_timeout (int timeout)
{
    installed_connect_timeout = timeout;
}

void
hev_socks5_set_tcp_timeout (int timeout)
{
    installed_handshake_timeout = timeout;
}

void
hev_socks5_set_udp_timeout (int timeout)
{
    installed_udp_timeout = timeout;
}

void
hev_socks5_set_timeout (HevSocks5 *socks5, int timeout)
{
    socks5->timeout = timeout;
}

int
hev_socks5_client_connect (HevSocks5Client *client, const char *address,
                           int port)
{
    assert (strcmp (address, "127.0.0.1") == 0);
    assert (port == expected_connect_port);
    hev_socks5_set_timeout (HEV_SOCKS5 (client), installed_connect_timeout);
    connect_calls++;
    if (fail_connect) {
        fail_connect = 0;
        return -1;
    }
    return 0;
}

int
hev_socks5_client_handshake (HevSocks5Client *client, int pipeline)
{
    (void)pipeline;
    assert (HEV_SOCKS5 (client)->timeout == configured_connect_timeout);
    hev_socks5_set_timeout (HEV_SOCKS5 (client), installed_handshake_timeout);
    assert (HEV_SOCKS5 (client)->timeout == configured_connect_timeout);
    if (HEV_SOCKS5 (client)->type == HEV_SOCKS5_TYPE_UDP_IN_TCP)
        hev_socks5_set_timeout (HEV_SOCKS5 (client), installed_udp_timeout);
    return 0;
}

void
hev_socks5_client_set_auth (HevSocks5Client *client, const char *user,
                            const char *pass)
{
    (void)client;
    (void)user;
    (void)pass;
}

HevConfigServer *
hev_config_get_socks5_server (void)
{
    return &server;
}

static void
run_session (HevSocks5Type type, int data_timeout)
{
    HevSocks5Client client = { 0 };
    client.base.base.klass = &object_class;
    client.base.type = type;
    expected_connect_port = type == HEV_SOCKS5_TYPE_TCP
        ? server.port
        : server.udp_port;
    expected_splice_timeout = data_timeout;
    hev_socks5_session_run (HEV_SOCKS5_SESSION (&client));
}

int
main (void)
{
    assert (hev_socks5_tunnel_main_inner (-1, NULL, NULL) == 0);
    assert (installed_connect_timeout == configured_connect_timeout);
    assert (installed_handshake_timeout == configured_connect_timeout);
    assert (installed_udp_timeout == configured_udp_data_timeout);

    run_session (HEV_SOCKS5_TYPE_TCP, configured_tcp_data_timeout);
    run_session (HEV_SOCKS5_TYPE_UDP_IN_TCP, configured_udp_data_timeout);
    assert (connect_calls == 2);
    assert (splice_calls == 2);

    fail_connect = 1;
    run_session (HEV_SOCKS5_TYPE_TCP, configured_tcp_data_timeout);
    assert (connect_calls == 3);
    assert (splice_calls == 2);
    return 0;
}

int hev_config_get_misc_udp_recv_buffer_size (void) { return 524288; }
int hev_config_get_misc_limit_nofile (void) { return 65535; }
int hev_config_get_misc_log_level (void) { return HEV_LOGGER_WARN; }
const char *hev_config_get_misc_log_file (void) { return NULL; }
const char *hev_config_get_misc_pid_file (void) { return NULL; }
int hev_config_init_from_file (const char *path) { (void)path; return 0; }
int hev_config_init_from_str (const unsigned char *value, unsigned int length) { (void)value; (void)length; return 0; }
void hev_config_fini (void) {}
void hev_socks5_set_udp_recv_buffer_size (int size) { (void)size; }
int hev_logger_init (HevLoggerLevel level, const char *path) { (void)level; (void)path; return 0; }
void hev_logger_fini (void) {}
void hev_logger_log (HevLoggerLevel level, const char *format, ...) { (void)level; (void)format; }
int hev_socks5_logger_init (HevSocks5LoggerLevel level, const char *path) { (void)level; (void)path; return 0; }
void hev_socks5_logger_fini (void) {}
int set_limit_nofile (int limit) { (void)limit; return 0; }
void run_as_daemon (const char *path) { (void)path; }
int hev_task_system_init (void) { return 0; }
void hev_task_system_fini (void) {}
void lwip_init (void) {}
int hev_socks5_tunnel_init (int fd) { (void)fd; return 0; }
int hev_socks5_tunnel_run (void) { return 0; }
void hev_socks5_tunnel_fini (void) {}
void hev_socks5_tunnel_stop (void) {}
void hev_task_wakeup (HevTask *task) { (void)task; }
int set_sock_mark (int fd, unsigned int mark) { (void)fd; (void)mark; return 0; }
void set_sock_tcp_fastopen (int fd, int enable) { (void)fd; (void)enable; }
