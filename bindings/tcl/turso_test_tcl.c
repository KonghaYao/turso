#include "turso_test_tcl.h"
#include "turso_test.h"
#include <stdint.h>

static char *pending_byte_trace(ClientData data, Tcl_Interp *interp,
                                const char *name, const char *element, int flags);

static int pending_byte_command(ClientData data, Tcl_Interp *interp,
                                int objc, Tcl_Obj *const objv[])
{
    (void)data;
    if (objc != 2) {
        Tcl_WrongNumArgs(interp, 1, objv, "PENDING-BYTE");
        return TCL_ERROR;
    }
    int value;
    if (Tcl_GetIntFromObj(interp, objv[1], &value) != TCL_OK) return TCL_ERROR;
    uint32_t previous = turso_test_pending_byte_get();
    if (value != 0) turso_test_pending_byte_set((uint32_t)value);
    Tcl_SetObjResult(interp, Tcl_NewIntObj((int32_t)previous));
    return TCL_OK;
}

static char *pending_byte_trace(ClientData data, Tcl_Interp *interp,
                                const char *name, const char *element, int flags)
{
    (void)data;
    (void)element;
    if (flags & TCL_INTERP_DESTROYED) return NULL;
    Tcl_SetVar2Ex(interp, name, NULL,
                 Tcl_NewIntObj((int32_t)turso_test_pending_byte_get()),
                 TCL_GLOBAL_ONLY);
    if (flags & TCL_TRACE_UNSETS) {
        Tcl_TraceVar(interp, name,
                     TCL_GLOBAL_ONLY | TCL_TRACE_READS | TCL_TRACE_WRITES | TCL_TRACE_UNSETS,
                     pending_byte_trace, NULL);
    }
    if (flags & TCL_TRACE_WRITES) return "linked variable is read-only";
    return NULL;
}

int TursoTest_Init(Tcl_Interp *interp)
{
    static const char *enabled[] = {
        "subquery", "compound", "view", "trigger", "pragma", "tempdb", "attach"
    };
    for (size_t index = 0; index < sizeof(enabled) / sizeof(enabled[0]); ++index) {
        if (Tcl_SetVar2(interp, "sqlite_options", enabled[index], "1",
                        TCL_GLOBAL_ONLY | TCL_LEAVE_ERR_MSG) == NULL) return TCL_ERROR;
    }
    if (Tcl_SetVar2(interp, "sqlite_options", "memdebug", "0",
                    TCL_GLOBAL_ONLY | TCL_LEAVE_ERR_MSG) == NULL) return TCL_ERROR;
    if (Tcl_SetVar2Ex(interp, "sqlite_pending_byte", NULL,
                      Tcl_NewIntObj((int32_t)turso_test_pending_byte_get()),
                      TCL_GLOBAL_ONLY | TCL_LEAVE_ERR_MSG) == NULL) return TCL_ERROR;
    if (Tcl_TraceVar(interp, "sqlite_pending_byte",
                     TCL_GLOBAL_ONLY | TCL_TRACE_READS | TCL_TRACE_WRITES | TCL_TRACE_UNSETS,
                     pending_byte_trace, NULL) != TCL_OK) return TCL_ERROR;
    Tcl_CreateObjCommand(interp, "sqlite3_test_control_pending_byte",
                         pending_byte_command, NULL, NULL);
    return TCL_OK;
}
